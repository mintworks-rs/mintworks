//! Bootstrap configuration: the few values that must come from the environment because
//! they are needed before the database is open. Everything else is a `settings` row —
//! see [`crate::settings`].

use std::{env, fmt};

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};

/// Values read from the environment at startup.
#[derive(Clone)]
pub struct Config {
	/// 32-byte root key. Every row in `secrets` is encrypted under a key derived from
	/// it. Never generated automatically: a missing `MASTER_KEY` is a refusal to start,
	/// because generating one silently would orphan every secret already stored.
	pub master_key: [u8; 32],
	pub db_path: String,
	pub data_dir: String,
	pub listen: String,
	pub base_url: String,
	/// Workers this process runs, overriding the `jobs.workers` setting. `Some(0)` means this
	/// process runs no jobs and reclaims nothing — per-process and therefore env, because a
	/// setting cannot differ between two processes sharing one database, and only one of them
	/// may run the runner (`crate::job::Runner::reclaim`).
	pub jobs_workers: Option<i64>,
}

impl Config {
	/// Reads the bootstrap environment. Startup-fatal by design — a half-configured
	/// process is worse than one that never starts. This is the single place in the
	/// framework where panicking is permitted.
	#[allow(clippy::expect_used, clippy::panic)]
	pub fn from_env() -> Self {
		let raw = env::var("MASTER_KEY").expect("MASTER_KEY must be set (32 bytes, base64)");
		let bytes = B64.decode(raw.trim()).expect("MASTER_KEY must be valid base64");
		// From the *slice*: `Vec<u8>`'s `TryInto` fails with `Err(Vec<u8>)` — the decoded key —
		// which `expect` would `Debug`-print on stderr, the leak the hand-written `Debug` below
		// prevents. `TryFromSliceError` names no length, so the message spells it out.
		let master_key: [u8; 32] = <[u8; 32]>::try_from(bytes.as_slice()).unwrap_or_else(|_| {
			panic!("MASTER_KEY must decode to exactly 32 bytes, got {}", bytes.len())
		});
		Self {
			master_key,
			db_path: env::var("DB_PATH").unwrap_or_else(|_| "data/saas.db".to_string()),
			data_dir: env::var("DATA_DIR").unwrap_or_else(|_| "data".to_string()),
			listen: env::var("LISTEN").unwrap_or_else(|_| "0.0.0.0:8080".to_string()),
			base_url: env::var("BASE_URL").unwrap_or_else(|_| "http://localhost:8080".to_string()),
			jobs_workers: env::var("JOBS_WORKERS")
				.ok()
				.map(|v| parse_workers(&v).unwrap_or_else(|e| panic!("{e}"))),
		}
	}
}

/// `JOBS_WORKERS`, held to the same `0..=64` as the `jobs.workers` setting it overrides.
///
/// A malformed value used to become `None`, which falls back to that setting — so the process
/// meant to declare "I run no jobs" started a `Runner` and a second `reclaim()`, flipping every
/// sibling process's `RUNNING` row back to `PENDING`.
fn parse_workers(raw: &str) -> Result<i64, String> {
	let n: i64 = raw
		.trim()
		.parse()
		.map_err(|_| format!("JOBS_WORKERS must be an integer 0..=64, got '{raw}'"))?;
	if !(0..=64).contains(&n) {
		return Err(format!("JOBS_WORKERS must be 0..=64, got {n}"));
	}
	Ok(n)
}

impl fmt::Debug for Config {
	/// Redacts `master_key` — `Config` ends up in tracing spans and error reports.
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("Config")
			.field("master_key", &"<redacted>")
			.field("db_path", &self.db_path)
			.field("data_dir", &self.data_dir)
			.field("listen", &self.listen)
			.field("base_url", &self.base_url)
			.field("jobs_workers", &self.jobs_workers)
			.finish()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_malformed_jobs_workers_is_refused_not_ignored() {
		assert_eq!(parse_workers(" 4 "), Ok(4));
		assert_eq!(parse_workers("0"), Ok(0));
		assert_eq!(parse_workers("64"), Ok(64));
		for bad in ["o", "", "-1", "65", "2.5"] {
			assert!(parse_workers(bad).is_err(), "{bad}");
		}
	}
}

// vim: ts=4
