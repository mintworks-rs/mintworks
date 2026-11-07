// SPDX-License-Identifier: MIT-0
//! Drives `app/tests.rn` through `Host::run_tests`, and the `greet-count` command through
//! `Host::run_command`, so `cargo test --all` covers both hooks.

#[path = "../src/main.rs"]
#[allow(dead_code)]
mod native;

use std::path::Path;

use mintworks_core::config::Config;

#[tokio::test]
async fn app_suite_passes_through_host() {
	let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
	assert!(native::host().run_tests(&dir, None).await.unwrap());
}

/// A filter that matches nothing is a failed run, not a green one.
#[tokio::test]
async fn a_filter_matching_no_case_fails_the_run() {
	let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
	assert!(!native::host().run_tests(&dir, Some("no-such-case")).await.unwrap());
}

/// A host command builds the real app (here on a temp database) and runs once against it.
#[tokio::test]
async fn a_host_command_runs_against_the_built_app() {
	let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
	let tmp = std::env::temp_dir().join(format!("native-module-command-{}", std::process::id()));
	let _ = std::fs::remove_dir_all(&tmp);
	let config = || Config {
		master_key: [7; 32],
		db_path: tmp.join("core.db").to_string_lossy().into_owned(),
		data_dir: tmp.to_string_lossy().into_owned(),
		listen: String::new(),
		base_url: String::new(),
		jobs_workers: None,
	};
	// `app.test_default` applies to `mintworks test` only; a command boots the plain app.
	let host = native::host().configure(|b| {
		b.extension(native::Greeting("szia".to_owned()))
			.setting_default("email.from", "noreply@native-module.test")
			.setting_default("email.smtp.host", "localhost")
	});

	let names = vec!["Anna".to_owned(), "Béla".to_owned()];
	host.run_command("greet-count", &dir, Some(config()), names).await.unwrap();
	assert!(host.run_command("no-such", &dir, Some(config()), vec![]).await.is_err());
	assert!(
		native::host()
			.command("test", |_, _| Box::pin(async { Ok(()) }))
			.run_command("test", &dir, Some(config()), vec![])
			.await
			.is_err()
	);
	let _ = std::fs::remove_dir_all(&tmp);
}

// vim: ts=4
