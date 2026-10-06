// SPDX-License-Identifier: MPL-2.0
//! `Agent`: the service handle. Every method takes `&Ctx` first and is confined to `ctx.org`, so
//! another org's thread or run is `E-CORE-NOTFOUND`, never 403. A `System` ctx with no org is the
//! application's own code and reaches any.

use std::{collections::VecDeque, sync::Arc};

use futures_util::{Stream, stream};
use mintworks_auth::store::AuthStore;
use mintworks_core::{
	App, ClResult, Ctx, Error,
	ctx::Actor,
	error::StatusCode,
	prelude::{OrgId, RunId, ThreadId},
};
use mintworks_memory::Memory;
use tokio::sync::broadcast::{self, error::RecvError};

use crate::{
	pool::{RunPool, store, threads},
	run::{self, RunSpec},
	skills::Skills,
	store::{AgentRunStore, EventKind, Message, NewRun, RunEvent, Thread},
	tool::Tools,
	tools::{memory_tools, search_tools, skill_tool},
};

/// No `RunPool` extension: the app did not declare `app.feature("agent")`.
pub const E_CONFIG: &str = "E-AGENT-CONFIG";

pub struct Agent {
	app: App,
}

impl Agent {
	pub fn new(app: App) -> Self {
		Self { app }
	}

	fn pool(&self) -> ClResult<RunPool> {
		self.app.extensions.get::<RunPool>().cloned().ok_or_else(|| {
			Error::coded(
				StatusCode::SERVICE_UNAVAILABLE,
				E_CONFIG,
				"no agent pool: the app did not declare app.feature(\"agent\")",
			)
		})
	}

	fn orgs(&self) -> ClResult<Arc<dyn AuthStore>> {
		self.app.extensions.get::<Arc<dyn AuthStore>>().cloned().ok_or_else(|| {
			Error::internal("mintworks-agent: no AuthStore was registered on the app")
		})
	}

	/// `None` for a `System` ctx with no org: unconfined.
	fn scope(ctx: &Ctx) -> ClResult<Option<i64>> {
		match (&ctx.actor, ctx.org_id) {
			(Actor::System { .. }, None) => Ok(None),
			_ => ctx.org().map(Some),
		}
	}

	/// The app's `Tools` extension (a script's `app.tool`s, or a consumer's own), plus the memory
	/// tools when `memory` is on and the search tools when a `SearchStore` is registered.
	fn tools(&self) -> Tools {
		let mut tools = self.app.extensions.get::<Tools>().cloned().unwrap_or_default();
		if let Some(memory) = self.app.extensions.get::<Arc<Memory>>() {
			for t in memory_tools(memory) {
				tools.add(t);
			}
		}
		if self.app.extensions.get::<Arc<dyn mintworks_search::SearchStore>>().is_some() {
			for t in search_tools(&self.app) {
				tools.add(t);
			}
		}
		if let Some(skills) = self.app.extensions.get::<Arc<Skills>>() {
			tools.add(skill_tool(skills));
		}
		tools
	}

	/// A new thread in `ctx`'s org; `subject` is the budget key its runs are charged to, `title`
	/// a display name the harness never reads.
	///
	/// # Errors
	/// `E-AUTH-FORBIDDEN` when `ctx` selected no org.
	pub async fn create_thread(
		&self,
		ctx: &Ctx,
		subject: Option<&str>,
		title: Option<&str>,
	) -> ClResult<Thread> {
		let org = self.orgs()?.org_by_id(ctx.org()?).await?.ok_or(Error::NotFound)?;
		threads(&self.app)?.thread_create(org.uid.as_str(), subject, title).await
	}

	/// `ctx`'s org's threads, newest first.
	///
	/// # Errors
	/// `E-AUTH-FORBIDDEN` when `ctx` selected no org.
	pub async fn threads(&self, ctx: &Ctx) -> ClResult<Vec<Thread>> {
		let org = self.orgs()?.org_by_id(ctx.org()?).await?.ok_or(Error::NotFound)?;
		let mut list = threads(&self.app)?.threads_list(org.uid.as_str()).await?;
		list.reverse();
		Ok(list)
	}

	/// Every message of `thread`, compacted or not, oldest first; confined to `ctx`'s org.
	/// `skill_read` results come back as stubs: skill text is operator instructions.
	pub async fn messages(&self, ctx: &Ctx, thread: &str) -> ClResult<Vec<Message>> {
		let (thread, _) = self.thread(ctx, thread).await?;
		let mut list = threads(&self.app)?.messages_all(thread.id).await?;
		let stubs = crate::tools::skill::skill_stubs(&list, &[]);
		for (m, stub) in list.iter_mut().zip(stubs) {
			if let Some(stub) = stub {
				m.content = stub;
			}
		}
		Ok(list)
	}

	/// The thread, confined to `ctx`'s org, with its org's internal id.
	async fn thread(&self, ctx: &Ctx, uid: &str) -> ClResult<(Thread, i64)> {
		let uid = ThreadId::parse(uid).map_err(|_| Error::NotFound)?;
		let thread = threads(&self.app)?.thread_get(&uid).await?.ok_or(Error::NotFound)?;
		let org = self
			.orgs()?
			.org_by_uid(&OrgId::from_trusted(thread.org.clone()))
			.await?
			.ok_or(Error::NotFound)?;
		match Self::scope(ctx)? {
			Some(id) if id != org.id => Err(Error::NotFound),
			_ => Ok((thread, org.id)),
		}
	}

	/// The run, confined to `ctx`'s org.
	async fn run(&self, ctx: &Ctx, uid: &str) -> ClResult<(Arc<dyn AgentRunStore>, crate::Run)> {
		let uid = RunId::parse(uid).map_err(|_| Error::NotFound)?;
		let runs = store(&self.app)?;
		let run = runs.run_get(&uid).await?.ok_or(Error::NotFound)?;
		match Self::scope(ctx)? {
			Some(id) if id != run.org_id => Err(Error::NotFound),
			_ => Ok((runs, run)),
		}
	}

	/// Start a run on `thread` and return its uid at once; the run proceeds in the pool as
	/// `ctx`'s actor, its role re-read from the DB now.
	///
	/// # Errors
	/// `E-AGENT-BUSY` (429) when the thread has a live run or the queue is full;
	/// `E-AUTH-FORBIDDEN` when the account is no longer a member of the thread's org.
	pub async fn start(&self, ctx: &Ctx, thread: &str, spec: &RunSpec) -> ClResult<RunId> {
		let pool = self.pool()?;
		let tools = self.tools();
		run::offered(&self.app, &tools, spec)?;
		let (thread, org_id) = self.thread(ctx, thread).await?;
		let account_id = ctx.actor.account_id();
		let role = match account_id {
			Some(acc) => match self.app.store.org_role(acc, org_id).await? {
				Some(r) => r.as_str(),
				None => {
					return Err(Error::coded(
						StatusCode::FORBIDDEN,
						"E-AUTH-FORBIDDEN",
						"not a member of the thread's org",
					));
				}
			},
			None => "SYSTEM",
		};
		let spec_json = serde_json::to_string(spec).map_err(|e| Error::internal(e.to_string()))?;
		let uid = RunId::generate();
		let new =
			NewRun { uid: &uid, thread: &thread.uid, org_id, account_id, role, spec: &spec_json };
		let app = self.app.clone();
		pool.submit(&self.app, &new, spec.org_limit, move |adm| run::execute(app, tools, adm))
			.await?;
		Ok(uid)
	}

	/// Trip the run's cancel token. A run that is already over, or live in no process, is a no-op.
	///
	/// # Errors
	/// `E-CORE-NOTFOUND` for a run outside `ctx`'s org.
	pub async fn cancel(&self, ctx: &Ctx, run: &str) -> ClResult<()> {
		let (_, run) = self.run(ctx, run).await?;
		self.pool()?.cancel(&run.uid);
		Ok(())
	}

	/// The run's events after `last_event_id`: the persisted ones, then the live tail until the
	/// final `done`/`error`. A run no longer live yields only what was persisted.
	///
	/// # Errors
	/// `E-CORE-NOTFOUND` for a run outside `ctx`'s org.
	pub async fn events(
		&self,
		ctx: &Ctx,
		run: &str,
		last_event_id: i64,
	) -> ClResult<impl Stream<Item = RunEvent> + Send + use<>> {
		let (runs, run) = self.run(ctx, run).await?;
		// Subscribe before the replay, or an event emitted between the two is lost.
		let rx = self.pool().ok().and_then(|p| p.get(&run.uid)).map(|l| l.tx.subscribe());
		let queue: VecDeque<RunEvent> = runs.events_after(run.id, last_event_id).await?.into();
		let tail = Tail { queue, rx, last: last_event_id, runs, run_id: run.id };
		Ok(stream::unfold(tail, Tail::next))
	}
}

struct Tail {
	queue: VecDeque<RunEvent>,
	rx: Option<broadcast::Receiver<RunEvent>>,
	last: i64,
	runs: Arc<dyn AgentRunStore>,
	run_id: i64,
}

impl Tail {
	async fn next(mut self) -> Option<(RunEvent, Self)> {
		loop {
			if let Some(ev) = self.queue.pop_front() {
				// The replay and the broadcast overlap: skip what was already sent.
				if ev.seq <= self.last {
					continue;
				}
				self.last = ev.seq;
				if matches!(ev.kind, EventKind::Done | EventKind::Error) {
					self.queue.clear();
					self.rx = None;
				}
				return Some((ev, self));
			}
			let rx = self.rx.as_mut()?;
			match rx.recv().await {
				Ok(ev) => self.queue.push_back(ev),
				// Lagged past the buffer, or the run ended: the DB has everything.
				Err(e) => {
					if matches!(e, RecvError::Closed) {
						self.rx = None;
					}
					match self.runs.events_after(self.run_id, self.last).await {
						Ok(v) => self.queue.extend(v),
						Err(e) => {
							tracing::warn!("agent event tail: {e}");
							return None;
						}
					}
				}
			}
		}
	}
}

// vim: ts=4
