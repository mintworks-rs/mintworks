// SPDX-License-Identifier: MPL-2.0
//! Script job and init handlers, registered onto the framework's one job runner.
//!
//! The `Ctx` a handler receives is minted here, never by the script: `Actor::System` is the most
//! privileged actor there is, so its `source` is interned to `&'static str` **at load time**
//! from the registered name and can carry nothing computed during a call.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mintworks_core::{
	App, Ctx,
	account_data::AccountDataHook,
	error::{ClResult, Error},
	ids::AccountId,
	job::{Job, Next, Runner},
	types::Timestamp,
};
use rune::{
	Hash, Value,
	runtime::{FromValue, Function, RuntimeError},
};

use crate::{
	ScriptCtx,
	routes::{Decl, Decls, intern},
	vm::Script,
};

/// How a declared job is scheduled.
pub enum Schedule {
	/// `app.job(kind, h)` — enqueued by something else.
	Once,
	/// `app.every(kind, secs, h)` — each completion re-enqueues the same payload.
	Every(i64),
	/// `app.next(kind, h)` — the handler decides its own next run.
	Next,
}

pub struct JobDecl {
	pub kind: String,
	pub entry: Hash,
	pub schedule: Schedule,
}

#[rune::function(instance)]
pub(crate) fn job(this: &Decl, kind: String, handler: Function) {
	this.push_job(JobDecl { kind, entry: handler.type_hash(), schedule: Schedule::Once });
}

#[rune::function(instance)]
pub(crate) fn every(this: &Decl, kind: String, every_secs: i64, handler: Function) {
	this.push_job(JobDecl {
		kind,
		entry: handler.type_hash(),
		schedule: Schedule::Every(every_secs),
	});
}

#[rune::function(instance)]
pub(crate) fn next(this: &Decl, kind: String, handler: Function) {
	this.push_job(JobDecl { kind, entry: handler.type_hash(), schedule: Schedule::Next });
}

#[rune::function(instance)]
pub(crate) fn on_init(this: &Decl, handler: Function) {
	this.push_init(handler.type_hash());
}

/// `app.on_event(kind, fn(ctx, event))` — `event` is `Event::to_json`, `kind` a variant name.
#[rune::function(instance)]
pub(crate) fn on_event(this: &Decl, kind: String, handler: Function) {
	this.push_event(kind, handler.type_hash());
}

/// `app.on_account_export(fn(ctx, account_uid))` — the returned value is the export's `"script"`
/// section.
#[rune::function(instance)]
pub(crate) fn on_account_export(this: &Decl, handler: Function) {
	this.push_account_hook(true, handler.type_hash());
}

/// `app.on_account_erase(fn(ctx, account_uid))` — runs before the account is anonymised, and
/// again on a retried erasure, so it must be idempotent.
#[rune::function(instance)]
pub(crate) fn on_account_erase(this: &Decl, handler: Function) {
	this.push_account_hook(false, handler.type_hash());
}

/// The `source` an init handler's `System` ctx carries. A job's is its registered kind — the
/// name that also appears in the `jobs` table — but `on_init` declares no name of its own.
pub const INIT_SOURCE: &str = "script.on_init";

/// Registers every declared job handler on `runner`.
///
/// # Errors
/// `E-SCRIPT-COMPILE` when a job kind will not intern, or is already registered.
pub fn register(
	runner: &mut Runner,
	app: &App,
	script: &Arc<Script>,
	decls: &Decls,
) -> ClResult<()> {
	for decl in &decls.jobs {
		// A Rust consumer may override a kind on purpose; a script silently replacing
		// `SWEEP_JOBS` or `SEND_EMAIL` is a typo, not a decision.
		if runner.contains(&decl.kind) {
			return Err(crate::error::compile(format!(
				"job kind '{}' is already registered by the framework",
				decl.kind
			)));
		}
		// One `&'static str` serves as both the runner's `kind` and the ctx's `source`: the
		// runner holds kinds for the process's life, so interning is what they need anyway.
		let kind = intern(&decl.kind)?;
		let (script, app, entry) = (Arc::clone(script), app.clone(), decl.entry);
		match decl.schedule {
			Schedule::Once => runner.register(kind, move |job| {
				let (script, app) = (Arc::clone(&script), app.clone());
				async move { run::<Discard>(&script, entry, &app, kind, job).await.map(|_| ()) }
			}),
			Schedule::Every(secs) => runner.register_periodic(kind, secs, move |job| {
				let (script, app) = (Arc::clone(&script), app.clone());
				async move { run::<Discard>(&script, entry, &app, kind, job).await.map(|_| ()) }
			}),
			Schedule::Next => runner.register_next(kind, move |job| {
				let (script, app) = (Arc::clone(&script), app.clone());
				async move { run::<Rescheduled>(&script, entry, &app, kind, job).await.map(|r| r.0) }
			}),
		}
	}
	Ok(())
}

/// A job or init handler acts for the deployment's root org. `Ctx::system` leaves `org_id`
/// unset and `Ctx::org()` then raises `E-AUTH-FORBIDDEN`, so without this *every* org-scoped
/// call fails from `on_init`. Script has no `with_org` of its own.
async fn system_ctx(app: &App, source: &'static str) -> ClResult<Ctx> {
	Ok(Ctx::system(source).with_org(app.store.root_org_id().await?))
}

async fn run<T: FromValue>(
	script: &Script,
	entry: Hash,
	app: &App,
	source: &'static str,
	job: Job,
) -> ClResult<T> {
	let payload = payload(&job)?;
	let ctx = ScriptCtx::new(app.clone(), system_ctx(app, source).await?);
	script.invoke(entry, (ctx, crate::routes::Arg(payload))).await
}

/// An empty payload is legitimately absent; a non-empty one that is not JSON is an error, not
/// a `Null` the handler runs against — swallowing it marked the row done instead of retrying.
fn payload(job: &Job) -> ClResult<serde_json::Value> {
	if job.payload.trim().is_empty() {
		return Ok(serde_json::Value::Null);
	}
	serde_json::from_str(&job.payload)
		.map_err(|e| crate::error::runtime(format!("job {}: payload is not JSON: {e}", job.kind)))
}

/// Runs every declared `on_init` handler, in declaration order.
///
/// # Errors
/// Whatever a handler raises; a failing init fails the boot, as `AppBuilder::on_init` intends.
pub async fn init(script: &Script, app: &App, entries: &[Hash]) -> ClResult<()> {
	let source = intern(INIT_SOURCE)?;
	for entry in entries {
		let ctx = ScriptCtx::new(app.clone(), system_ctx(app, source).await?);
		let _: Discard = script.invoke(*entry, (ctx,)).await?;
	}
	Ok(())
}

/// The `source` an event handler's `System` ctx carries.
pub const EVENT_SOURCE: &str = "script.on_event";

/// Runs every `app.on_event` handler declared for `ev`'s kind, in declaration order. Called
/// by `mintworks_core::event::emit` after the handlers registered before it, so an error here is
/// logged there, never raised.
///
/// # Errors
/// The first handler's error; the handlers after it do not run.
pub async fn dispatch(
	script: &Script,
	app: &App,
	events: &[(String, Hash)],
	ev: mintworks_core::event::Event,
) -> ClResult<()> {
	let kind = ev.kind();
	let payload = ev.to_json();
	for (_, entry) in events.iter().filter(|(k, _)| k == kind) {
		let ctx = ScriptCtx::new(app.clone(), system_ctx(app, intern(EVENT_SOURCE)?).await?);
		let _: Discard = script.invoke(*entry, (ctx, crate::routes::Arg(payload.clone()))).await?;
	}
	Ok(())
}

/// The `source` an account hook's `System` ctx carries.
pub const ACCOUNT_SOURCE: &str = "script.account_data";

/// The one `AccountDataHook` a script registers, fanning out to its declared handlers.
pub struct AccountHooks {
	pub script: Arc<Script>,
	pub export: Option<Hash>,
	pub erase: Option<Hash>,
	/// Set by `ScriptApp`'s `on_init`: the hook is registered on the builder, before an `App`
	/// exists.
	pub app: Arc<OnceLock<App>>,
}

impl AccountHooks {
	async fn call<T: FromValue>(&self, entry: Hash, acc: &AccountId) -> ClResult<T> {
		let app = self
			.app
			.get()
			.ok_or_else(|| Error::internal("script account hook called before on_init"))?;
		let ctx = ScriptCtx::new(app.clone(), system_ctx(app, intern(ACCOUNT_SOURCE)?).await?);
		self.script.invoke(entry, (ctx, acc.to_string())).await
	}
}

#[async_trait]
impl AccountDataHook for AccountHooks {
	fn name(&self) -> &'static str {
		"script"
	}

	async fn export(&self, acc: &AccountId) -> ClResult<serde_json::Value> {
		match self.export {
			Some(entry) => self.call::<Json>(entry, acc).await?.0,
			None => Ok(serde_json::Value::Null),
		}
	}

	async fn erase(&self, acc: &AccountId) -> ClResult<()> {
		match self.erase {
			Some(entry) => self.call::<Discard>(entry, acc).await.map(|_| ()),
			None => Ok(()),
		}
	}
}

/// A handler's return value as JSON, converted inside `FromValue` for the reason `Discard` gives.
struct Json(ClResult<serde_json::Value>);

impl FromValue for Json {
	fn from_value(value: Value) -> Result<Self, RuntimeError> {
		Ok(Self(crate::value::to_json(&value)))
	}
}

/// A handler's return value, thrown away. `rune::Value` is not `Send`, so the future of an
/// `invoke::<Value>` cannot be the `Send` one the job runner needs.
struct Discard;

impl FromValue for Discard {
	fn from_value(_: Value) -> Result<Self, RuntimeError> {
		Ok(Self)
	}
}

/// What an `app.next` handler returns: a unix timestamp to run again at, or `()` for done.
struct Rescheduled(Next);

impl FromValue for Rescheduled {
	fn from_value(value: Value) -> Result<Self, RuntimeError> {
		// Only `()` means done: any other non-integer is an error, never a silently ended chain.
		if crate::api::is_unit(&value) {
			return Ok(Self(Next::Done));
		}
		Ok(Self(Next::Again { at: Timestamp(i64::from_value(value)?) }))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn job(payload: &str) -> Job {
		Job { id: 1, kind: "k".to_owned(), payload: payload.to_owned(), attempts: 0 }
	}

	#[test]
	fn a_malformed_payload_is_an_error() {
		assert!(payload(&job("")).unwrap().is_null());
		assert!(payload(&job("  ")).unwrap().is_null());
		assert_eq!(payload(&job(r#"{"a":1}"#)).unwrap(), serde_json::json!({ "a": 1 }));
		assert_eq!(payload(&job("{not json")).unwrap_err().parts().1, crate::E_RUNTIME);
	}

	#[test]
	fn a_non_timestamp_reschedule_is_an_error() {
		let at = |v: Value| Rescheduled::from_value(v).map(|r| r.0);
		assert!(matches!(at(Value::from(90i64)).unwrap(), Next::Again { at: Timestamp(90) }));
		assert!(matches!(at(Value::from(())).unwrap(), Next::Done));
		assert!(at(rune::to_value("90").unwrap()).is_err());
	}
}

// vim: ts=4
