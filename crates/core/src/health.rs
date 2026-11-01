// SPDX-License-Identifier: MPL-2.0
//! Liveness and readiness probes, and the token-gated Prometheus scrape. All sit outside `/api`.

use std::fmt::Write as _;
use std::net::IpAddr;

use axum::Extension;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};

use crate::app::App;
use crate::auth_mw::ClientIp;
use crate::error::{ClResult, Error};
use crate::ratelimit;
use crate::types::Timestamp;

/// The secret `GET /metrics` compares the bearer against (env `METRICS_TOKEN`).
pub const METRICS_TOKEN_KEY: &str = "metrics.token";

/// The probes. [`crate::AppBuilder::run`] mounts this itself — an application that merges it
/// too gets axum's "Overlapping method route" panic at process start, which is why there is
/// no `mintworks_core::routes` re-export. Public so a test (or a consumer serving its own router)
/// can mount the probes deliberately.
pub fn public() -> Router<App> {
	Router::new()
		.route("/healthz", get(healthz))
		.route("/readyz", get(readyz))
		.route("/metrics", get(metrics))
}

/// Liveness only — the process is up. No DB access, cheap enough for a per-second probe.
async fn healthz() -> Json<Value> {
	Json(json!({ "status": "ok" }))
}

/// Readiness. Actually checks the DB and the job runner.
async fn readyz(State(app): State<App>) -> Response {
	// A read error is reported rather than swallowed into version 0.
	let (db_ok, db_version) = match app.store.db_version().await {
		Ok(v) => (true, v),
		Err(e) => {
			tracing::warn!(error = %e, "readyz: db check failed");
			(false, 0)
		}
	};
	let jobs_ok = app.jobs_alive();
	let ok = db_ok && jobs_ok;

	let mut body = json!({
		"status": if ok { "ok" } else { "fail" },
		"db": if db_ok { "ok" } else { "fail" },
		"jobs": if jobs_ok { "ok" } else { "fail" },
		"dbVersion": db_version,
		"startedAt": app.started_at.to_rfc3339(),
	});
	if ok {
		return (StatusCode::OK, Json(body)).into_response();
	}
	if let Value::Object(map) = &mut body {
		map.insert("errCode".to_owned(), Value::from("E-CORE-UNAVAILABLE"));
	}
	(StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response()
}

/// `GET /metrics`: Prometheus text, or the [`metrics_text`] error.
async fn metrics(
	State(app): State<App>,
	ip: Option<Extension<ClientIp>>,
	headers: HeaderMap,
) -> ClResult<Response> {
	let bearer = headers
		.get(AUTHORIZATION)
		.and_then(|v| v.to_str().ok())
		.and_then(|v| v.strip_prefix("Bearer "))
		.unwrap_or("");
	let text = metrics_text(&app, ip.map(|Extension(ClientIp(ip))| ip), bearer).await?;
	Ok(([(CONTENT_TYPE, "text/plain; version=0.0.4")], text).into_response())
}

/// The scrape's own failed-token bucket, not `AUTH_FAILED`: NAT'd login failures sharing the
/// scraper's address must not 429 it.
const METRICS_AUTH: &str = "metrics.auth";

/// Prometheus text for job-queue health. `NotFound` while `metrics.token` is unset, so the
/// route is off by default; otherwise `bearer` must match or 401 `E-AUTH-TOKEN`, charged to the
/// IP's `metrics.auth` bucket, which once empty refuses every scrape from that IP. Only
/// aggregates by job kind and status go out — no org ids, payloads or error text.
pub async fn metrics_text(app: &App, ip: Option<IpAddr>, bearer: &str) -> ClResult<String> {
	let Some(token) = app.secrets.get(METRICS_TOKEN_KEY).await?.filter(|t| !t.is_empty()) else {
		return Err(Error::NotFound);
	};
	let key = ip.map(ratelimit::bucket_key);
	// Checked before the compare: charging only the 401 would still answer a right guess 200.
	if let Some(key) = &key
		&& app.limits.exhausted(METRICS_AUTH, key)
	{
		app.limits.check(&app.settings, METRICS_AUTH, key).await?;
	}
	if !crate::crypto::ct_eq(bearer.as_bytes(), &token) {
		if let Some(key) = &key {
			app.limits.check(&app.settings, METRICS_AUTH, key).await?;
		}
		return Err(Error::coded(
			StatusCode::UNAUTHORIZED,
			"E-AUTH-TOKEN",
			"metrics token required",
		));
	}

	let rows = app.store.job_counts().await?;
	let now = Timestamp::now().0;
	let mut out =
		String::from("# TYPE mintworks_up gauge\nmintworks_up 1\n# TYPE mintworks_jobs gauge\n");
	for r in &rows {
		let _ = writeln!(
			out,
			"mintworks_jobs{{kind=\"{}\",status=\"{}\"}} {}",
			label(&r.kind),
			label(&r.status),
			r.n
		);
	}
	out.push_str("# TYPE mintworks_jobs_oldest_pending_seconds gauge\n");
	for r in &rows {
		if let Some(at) = r.oldest_pending_at {
			// Not yet due counts as zero wait, not a negative age.
			let _ = writeln!(
				out,
				"mintworks_jobs_oldest_pending_seconds{{kind=\"{}\"}} {}",
				label(&r.kind),
				(now - at.0).max(0)
			);
		}
	}
	Ok(out)
}

/// Prometheus label-value escaping. Kinds are the programmer's, but a script names its own.
fn label(v: &str) -> String {
	v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

// vim: ts=4
