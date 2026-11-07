// SPDX-License-Identifier: MPL-2.0
//! The access matrix: every framework route, through the composed router, against every
//! subject.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// The oracle and policy layers read what the harness builds.
#![allow(dead_code)]

mod curated;
mod drift;
mod fixture;
mod levels;
mod objects;
mod oracle;
mod report;
mod routes;
mod subjects;

use axum::http::{Method, StatusCode};
use fixture::{call, fixture, req};
use objects::{ObjKind, OrgTag};

#[tokio::test]
async fn smoke() {
	let fx = fixture().await;
	let r = call(&fx.router, req(Method::GET, "/healthz", None, None)).await;
	assert_eq!(r.status, StatusCode::OK);
	let owner = fx.subject("owner_a").bearer.as_deref();
	let r = call(&fx.router, req(Method::GET, "/api/auth/me", owner, None)).await;
	assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
}

/// `axum::Json` answers a bad body in plain text, so a client finds no `errCode` to act on.
#[tokio::test]
async fn a_rejected_body_keeps_the_error_envelope() {
	let fx = fixture().await;
	let filing = &fx.obj(ObjKind::Filing, OrgTag::A).key;
	let resolve = format!("/api/admin/nav-submissions/{filing}/resolve");
	let cases = [
		("operator", resolve.as_str()),
		("owner_a", "/api/api-keys"),
		("operator", "/api/admin/payments"),
	];
	for (who, uri) in cases {
		let bearer = fx.subject(who).bearer.as_deref();
		let r = call(&fx.router, req(Method::POST, uri, bearer, Some(serde_json::json!({})))).await;
		assert_eq!(r.status, StatusCode::BAD_REQUEST, "{uri}: {:?}", r.body);
		let body = r.body.unwrap_or_default();
		assert!(body["error"]["errCode"].is_string(), "{uri}: {body}");
	}
}

/// Always on: it checks the route table, not policy.
#[tokio::test]
async fn drift() {
	drift::check(fixture().await).await;
}

/// Always on: `http.ts` must not preflight a refresh before a public auth POST.
#[test]
fn no_preflight_matches_public_auth_posts() {
	drift::no_preflight();
}

/// Always on: it checks the curated table's coverage, not policy.
#[tokio::test]
async fn self_mut_is_curated() {
	curated::self_mut_is_curated().await;
}

#[tokio::test]
async fn level_read() {
	levels::level_read().await;
}

#[tokio::test]
async fn level_mutate() {
	levels::level_mutate().await;
}

#[tokio::test]
async fn curated() {
	curated::curated().await;
}

// vim: ts=4
