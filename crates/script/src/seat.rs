// SPDX-License-Identifier: MPL-2.0
//! `seat::` — seat assignments against a `limit` key of an explicit org, through
//! `mintworks_entitle::Entitle`.

use rune::{ContextError, Module, Value, runtime::Ref};
use serde::{Deserialize, Serialize};

use crate::{
	ctx::ScriptCtx,
	entitle::handle,
	error::R,
	tx,
	value::{ScriptError, arg, wire},
};

/// `#{org, key, account}` of `seat::assign`/`seat::release`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SeatArg {
	org: String,
	key: String,
	account: String,
}

/// `#{org, key}` of `seat::list`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SeatsArg {
	org: String,
	key: String,
}

/// A `seat::list` row.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SeatRow<'a> {
	account: &'a str,
	assigned_at: i64,
}

/// A `seat::of_account` row.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountSeatRow<'a> {
	org: &'a str,
	key: &'a str,
	assigned_at: i64,
}

/// `seat::assign(ctx, #{org, key, account})` — `true` when newly seated, `false` when it
/// already was; 409 `E-ENT-SEATS-FULL` past the org's limit. A core-DB write, so it joins
/// `tx::with`.
#[rune::function]
async fn assign(c: Ref<ScriptCtx>, req: Value) -> R<bool> {
	tx::outside_app_tx("seat::assign")?;
	let SeatArg { org, key, account } = arg("seat::assign", &req)?;
	let (e, ctx) = handle(c)?;
	e.seat_assign(&ctx, &org, &key, &account).await.map_err(ScriptError)
}

/// `seat::release(ctx, #{org, key, account})` — `false` (a no-op) when no seat was held. A
/// core-DB write, so it joins `tx::with`.
#[rune::function]
async fn release(c: Ref<ScriptCtx>, req: Value) -> R<bool> {
	tx::outside_app_tx("seat::release")?;
	let SeatArg { org, key, account } = arg("seat::release", &req)?;
	let (e, ctx) = handle(c)?;
	e.seat_release(&ctx, &org, &key, &account).await.map_err(ScriptError)
}

/// `seat::list(ctx, #{org, key})` → `[#{account, assignedAt}]`, oldest first; unix seconds.
#[rune::function]
async fn list(c: Ref<ScriptCtx>, req: Value) -> R<Value> {
	let SeatsArg { org, key } = arg("seat::list", &req)?;
	let (e, ctx) = handle(c)?;
	let rows = e.seats(&ctx, &org, &key).await.map_err(ScriptError)?;
	let out: Vec<_> = rows
		.iter()
		.map(|(acc, at)| SeatRow { account: acc.as_str(), assigned_at: at.0 })
		.collect();
	wire(out)
}

/// `seat::of_account(ctx, account)` → `[#{org, key, assignedAt}]` — for the account itself,
/// an operator or `System`; anyone else gets `E-CORE-NOTFOUND`.
#[rune::function]
async fn of_account(c: Ref<ScriptCtx>, account: String) -> R<Value> {
	let (e, ctx) = handle(c)?;
	let rows = e.seats_of_account(&ctx, &account).await.map_err(ScriptError)?;
	let out: Vec<_> = rows
		.iter()
		.map(|(org, key, at)| AccountSeatRow { org: org.as_str(), key, assigned_at: at.0 })
		.collect();
	wire(out)
}

/// Registers `seat::`.
///
/// # Errors
/// Whatever Rune raises registering a function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["seat"])?;
	m.function_meta(assign)?;
	m.function_meta(release)?;
	m.function_meta(list)?;
	m.function_meta(of_account)?;
	Ok(m)
}

// vim: ts=4
