// SPDX-License-Identifier: MIT-0
//! Drives `app/tests.rn` through `Host::run_tests`, so `cargo test --all` covers the hook.

#[path = "../src/main.rs"]
#[allow(dead_code)]
mod native;

use std::path::Path;

#[tokio::test]
async fn app_suite_passes_through_host() {
	let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("app");
	assert!(native::host().run_tests(&dir, None).await.unwrap());
}

// vim: ts=4
