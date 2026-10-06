// SPDX-License-Identifier: MPL-2.0
//! The level layers: every `RouteSpec` × every subject × the object in A and its twin in B,
//! judged by the oracle.

use axum::http::Method;
use futures_util::stream::{self, StreamExt};
use serde_json::Value;

use crate::fixture::{Fixture, Resp, call, fixture, req};
use crate::objects::{Obj, OrgTag, seed};
use crate::oracle::expected;
use crate::report::Report;
use crate::routes::{RouteSpec, routes};
use crate::subjects::{Cred, Kind, Subject, clone_subject};

const MAX_PAGES: usize = 100;

/// A path parameter with no store seam (QR session, WebAuthn credential) could only ever answer
/// NOTFOUND, so those routes are curated rows instead.
fn skipped(r: &RouteSpec) -> bool {
	r.object.is_none() && r.path.contains('{')
}

fn mutating(r: &RouteSpec) -> bool {
	r.method != Method::GET && r.method != Method::HEAD
}

fn label(r: &RouteSpec) -> String {
	format!("{} {}", r.method, r.path)
}

fn path(r: &RouteSpec, o: Option<&Obj>) -> String {
	// A required query: `drift` compares `r.path` with the mounted literal, so it cannot carry one.
	let query = if r.path == "/api/pow/challenge" { "?scope=register" } else { "" };
	let p = r
		.path
		.split('/')
		.map(|seg| match o {
			_ if seg == "{lineNo}" => "1",
			Some(o) if seg.starts_with('{') => o.key.as_str(),
			_ => seg,
		})
		.collect::<Vec<_>>()
		.join("/");
	p + query
}

fn encode(s: &str) -> String {
	s.bytes()
		.map(|c| {
			if c.is_ascii_alphanumeric() || b"-_.~".contains(&c) {
				char::from(c).to_string()
			} else {
				format!("%{c:02X}")
			}
		})
		.collect()
}

/// Pages a listing through `?cursor=` until `nextCursor` is absent or null, looking for `key` as
/// any top-level field of an item (the id field differs per kind: `uid`, `code`, `accountUid`).
pub(crate) async fn listed(
	fx: &Fixture,
	uri: &str,
	bearer: Option<&str>,
	first: &Resp,
	key: &str,
) -> Result<bool, String> {
	let mut body = first.body.clone();
	for _ in 0..MAX_PAGES {
		let b = body.ok_or("listing body is not JSON")?;
		let items = b.get("items").unwrap_or(&b).as_array().ok_or("listing has no items array")?;
		let hit = |it: &Value| it.as_object().is_some_and(|m| m.values().any(|v| v == key));
		if items.iter().any(hit) {
			return Ok(true);
		}
		let Some(cur) = b.get("nextCursor").and_then(Value::as_str) else {
			return Ok(false);
		};
		let next = format!("{uri}?cursor={}", encode(cur));
		let r = call(&fx.router, req(Method::GET, &next, bearer, None)).await;
		if !r.status.is_success() {
			return Err(format!("next page answered {}", r.status));
		}
		body = r.body;
	}
	Err(format!("more than {MAX_PAGES} pages"))
}

/// GET routes, concurrently. An object route runs on the A object and its B twin; a pure listing
/// runs once per listed object and records its presence.
pub async fn level_read() {
	let fx = fixture().await;
	let specs: &'static [RouteSpec] = Box::leak(routes().into_boxed_slice());
	let mut cells = Vec::new();
	for r in specs.iter().filter(|r| !mutating(r) && !skipped(r)) {
		let objs = match r.object.or(r.lists) {
			Some(k) => vec![Some(fx.obj(k, OrgTag::A)), Some(fx.obj(k, OrgTag::B))],
			None => vec![None],
		};
		for o in objs {
			cells.extend(fx.subjects.iter().map(|s| (r, s, o)));
		}
	}
	let mut report = Report::new("level_read", cells.len());
	let done: Vec<_> = stream::iter(cells)
		.map(|(r, s, o)| async move {
			let uri = path(r, o.filter(|_| r.object.is_some()));
			let bearer = s.bearer.as_deref();
			let resp = call(&fx.router, req(r.method.clone(), &uri, bearer, None)).await;
			let seen = match o {
				Some(o) if r.object.is_none() && resp.status.is_success() => {
					Some(listed(fx, &uri, bearer, &resp, &o.key).await)
				}
				_ => None,
			};
			(r, s, o, resp, seen)
		})
		.buffer_unordered(16)
		.collect()
		.await;
	for (r, s, o, resp, seen) in done {
		let (route, obj) = (label(r), o.map(|o| o.name.as_str()));
		let seen = match seen {
			Some(Err(e)) => {
				report.error(&route, &e, s.name, obj);
				continue;
			}
			Some(Ok(b)) => Some(b),
			None => None,
		};
		report.check(&route, s.name, obj, &expected(&s.facts, r, o), &resp, seen);
	}
	report.finish();
}

pub(crate) enum Caller {
	Itself,
	Clone,
	Skip,
}

/// Who runs a `self_mut` cell. A live session runs as a disposable clone; a credential that never
/// reaches a handler (dead token, suspended account, a key on an unscoped route) runs as itself.
/// Impersonation and a dead membership or org reach handlers as a shared account no clone can
/// reproduce, so those cells are curated rows.
pub(crate) fn caller(r: &RouteSpec, s: &Subject) -> Caller {
	let f = &s.facts;
	let key = matches!(f.kind, Kind::Key { .. });
	if !r.self_mut || f.cred != Cred::Valid || !f.account_live || (key && r.scope.is_none()) {
		Caller::Itself
	} else if f.kind == Kind::Session && f.org_live && f.member_live {
		Caller::Clone
	} else {
		Caller::Skip
	}
}

/// Mutating routes, serially, each cell on a freshly seeded object (in A, then its twin in B).
pub async fn level_mutate() {
	let fx = fixture().await;
	let specs = routes();
	let muts: Vec<&RouteSpec> = specs.iter().filter(|r| mutating(r) && !skipped(r)).collect();
	let tags: fn(&RouteSpec) -> &'static [Option<OrgTag>] =
		|r| if r.object.is_some() { &[Some(OrgTag::A), Some(OrgTag::B)] } else { &[None] };
	let planned = muts
		.iter()
		.map(|r| {
			let run = fx.subjects.iter().filter(|s| !matches!(caller(r, s), Caller::Skip));
			run.count() * tags(r).len()
		})
		.sum();
	let mut report = Report::new("level_mutate", planned);
	for r in muts {
		let route = label(r);
		for s in &fx.subjects {
			let clone = match caller(r, s) {
				Caller::Skip => continue,
				Caller::Itself => false,
				Caller::Clone => true,
			};
			for tag in tags(r) {
				let obj = match (r.object, tag) {
					(Some(k), Some(t)) => Some(seed(fx, k, *t).await),
					_ => None,
				};
				let twin = if clone { Some(clone_subject(fx, s).await) } else { None };
				let s = twin.as_ref().unwrap_or(s);
				let body = (r.body)(obj.as_ref().unwrap_or(&fx.objs[0]));
				let uri = path(r, obj.as_ref());
				let resp =
					call(&fx.router, req(r.method.clone(), &uri, s.bearer.as_deref(), body)).await;
				let exp = expected(&s.facts, r, obj.as_ref());
				report.check(
					&route,
					s.name,
					obj.as_ref().map(|o| o.name.as_str()),
					&exp,
					&resp,
					None,
				);
			}
		}
	}
	report.finish();
}

// vim: ts=4
