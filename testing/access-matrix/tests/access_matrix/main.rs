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

#[tokio::test]
async fn smoke() {
	let fx = fixture().await;
	let r = call(&fx.router, req(Method::GET, "/healthz", None, None)).await;
	assert_eq!(r.status, StatusCode::OK);
	let owner = fx.subject("owner_a").bearer.as_deref();
	let r = call(&fx.router, req(Method::GET, "/api/auth/me", owner, None)).await;
	assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
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
