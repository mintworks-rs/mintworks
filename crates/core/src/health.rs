//! Liveness and readiness probes. Both sit outside `/api`.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};

use crate::app::App;

/// The probes. [`crate::AppBuilder::run`] mounts this itself — an application that merges it
/// too gets axum's "Overlapping method route" panic at process start, which is why there is
/// no `mintworks_core::routes` re-export. Public so a test (or a consumer serving its own router)
/// can mount the probes deliberately.
pub fn public() -> Router<App> {
	Router::new().route("/healthz", get(healthz)).route("/readyz", get(readyz))
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

// vim: ts=4
