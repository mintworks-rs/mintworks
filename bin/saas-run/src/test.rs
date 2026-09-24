//! `saas-run test <app-dir>` — the Rune `#[test]` runner.
//!
//! Every case gets its own real file database, so one case's rows are never another's fixture.
//! That means one build per case: the application is composed, migrated and seeded from scratch
//! each time, and the router the case drives is the one `AppBuilder::into_service` composes —
//! the same stack, rate limiter included, that production serves.

use std::{path::Path, sync::Arc};

use saas_core::{App, ClResult, config::Config, error::Error};
use saas_script::testing::{self, Harness};
use serde_json::{Value as Json, json};
use store_adapter_sqlite::SqliteStore;

use crate::app;

/// The seeded account and org. Fixed uids so a script test can name them without reading them
/// back; `test::session()` hands both to the case anyway.
const ACCOUNT: &str = "acc_01JCZ5X8K9N7QW3M6R2T4V8Y0A";
const ORG: &str = "org_01JCZ5X8K9N7QW3M6R2T4V8Y0C";
/// The foreign org: same owner, its own token, nothing seeded into it. A cross-org test needs
/// a second org that really exists, or it proves the shape of `E-CORE-NOTFOUND` and not tenancy.
const ORG_B: &str = "org_01JCZ5X8K9N7QW3M6R2T4V8Y0D";

/// A real file database in a temp dir, never `sqlite::memory:` — an in-memory URL gives each
/// connection its own database, so two connections would never contend for the write lock.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let slug: String =
			name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
		let dir = std::env::temp_dir().join(format!("saas-run-test-{}-{slug}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}

	fn config(&self) -> Config {
		Config {
			master_key: [7; 32],
			db_path: self.0.join("test.db").to_string_lossy().into_owned(),
			data_dir: self.0.to_string_lossy().into_owned(),
			listen: String::new(),
			base_url: String::new(),
			jobs_workers: None,
		}
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

/// Discovers the cases, runs each against its own database, and answers non-zero on any failure.
///
/// # Errors
/// Whatever compiling or composing the application raised. A *failing test* is not an error
/// here — it is reported and counted, and the process exit code carries the verdict.
pub async fn run(dir: &Path, filter: Option<&str>) -> ClResult<bool> {
	// One throwaway build just to list the cases. A case must not see another's rows, so the
	// app is rebuilt per case, and the list has to exist before the loop that rebuilds it.
	let probe = TmpDb::new("discover");
	let (_, _, found, _) = app::build_tests(dir, probe.config()).await?;
	let names: Vec<String> = found
		.into_iter()
		.map(|t| t.name)
		.filter(|n| filter.is_none_or(|f| n.contains(f)))
		.collect();
	drop(probe);

	let (mut passed, mut failed) = (0_usize, 0_usize);
	for name in &names {
		let db = TmpDb::new(name);
		let (builder, script, tests, store) = app::build_tests(dir, db.config()).await?;
		let Some(test) = tests.iter().find(|t| &t.name == name) else { continue };

		let (app, router) = builder.into_service().await?;
		let session = seed(&app, &store).await?;
		let harness = Arc::new(Harness { router, session });

		match testing::run_test(&script, test, harness).await {
			Ok(()) => {
				passed += 1;
				println!("ok   {name}");
			}
			Err(e) => {
				failed += 1;
				let (status, code) = e.parts();
				println!("FAIL {name}\n       {status} {code}: {e}");
			}
		}
		app.shutdown_jobs().await;
	}

	println!("\n{passed} passed, {failed} failed");
	Ok(failed == 0)
}

/// The chain every authenticated route needs before a case can call one: an `ACTIVE` account,
/// an org under the root with a membership, and both gating consents accepted.
///
/// `consents_required` fails closed — a gating kind published in no locale gates every route —
/// so the documents and the account's acceptance of them are part of the minimum, not extras.
/// Written as SQL rather than through the service handles because registration, activation and
/// consent publication are three features' worth of fixture for what `verify` reads as three
/// columns; `examples/booking/backend/tests/flow.rs` seeds the same shape the same way.
async fn seed(app: &App, store: &SqliteStore) -> ClResult<Json> {
	let db = |sql: &'static str| sqlx::query(sql).execute(store.write_pool());
	let fail = |e: sqlx::Error| Error::internal(format!("seeding the test database: {e}"));

	db("INSERT INTO accounts (id, uid, email, status, created_at)
	    VALUES (1, 'acc_01JCZ5X8K9N7QW3M6R2T4V8Y0A', 'test@example.test', 'ACTIVE', 0)")
	.await
	.map_err(fail)?;
	db("INSERT INTO orgs (uid, parent_id, kind, name, owner_account_id, created_at)
	    VALUES ('org_01JCZ5X8K9N7QW3M6R2T4V8Y0C',
	            (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'Test', 1, 0)")
	.await
	.map_err(fail)?;
	db("INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
	    VALUES ((SELECT id FROM orgs WHERE uid = 'org_01JCZ5X8K9N7QW3M6R2T4V8Y0C'),
	            1, 'OWNER', 0, 0)")
	.await
	.map_err(fail)?;
	db("INSERT INTO orgs (uid, parent_id, kind, name, owner_account_id, created_at)
	    VALUES ('org_01JCZ5X8K9N7QW3M6R2T4V8Y0D',
	            (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'Other', 1, 0)")
	.await
	.map_err(fail)?;
	db("INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
	    VALUES ((SELECT id FROM orgs WHERE uid = 'org_01JCZ5X8K9N7QW3M6R2T4V8Y0D'),
	            1, 'OWNER', 0, 0)")
	.await
	.map_err(fail)?;
	// Seller and catalogue writes gate on the *resolved seller's* org (`require_seller_role`),
	// and a script `on_init` can only seed the root's — while `org_role` walks ancestors of the
	// queried org, never down. Without this row every write from `test::session()` is 403.
	sqlx::query(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES (?, 1, 'OWNER', 0, 0)",
	)
	.bind(app.store.root_org_id().await?)
	.execute(store.write_pool())
	.await
	.map_err(fail)?;
	// No literal ids, and a document only where `<app-dir>/legal/*.md` published none. The consent
	// names whichever document `current_legal_doc` picks (exact locale, else newest): a consent
	// against any other row still gates.
	for kind in ["TOS", "PRIVACY"] {
		sqlx::query(
			"INSERT INTO legal_docs
			   (kind, locale, version, title, body, sha256, effective_from, created_at)
			 SELECT ?, (SELECT locale FROM accounts WHERE id = 1), 'v1', 't', 'b', 'deadbeef', 0, 0
			  WHERE NOT EXISTS (SELECT 1 FROM legal_docs WHERE kind = ?)",
		)
		.bind(kind)
		.bind(kind)
		.execute(store.write_pool())
		.await
		.map_err(fail)?;
		sqlx::query(
			"INSERT INTO consents
			   (account_id, kind, legal_doc_id, doc_version, doc_sha256, granted, at)
			 SELECT 1, kind, id, version, sha256, 1, 0 FROM legal_docs
			  WHERE kind = ?
			  ORDER BY (locale = (SELECT locale FROM accounts WHERE id = 1)) DESC,
			           effective_from DESC, id DESC
			  LIMIT 1",
		)
		.bind(kind)
		.execute(store.write_pool())
		.await
		.map_err(fail)?;
	}

	Ok(json!({
		"token": token(app, ORG).await?,
		"accountUid": ACCOUNT,
		"orgUid": ORG,
		"tokenB": token(app, ORG_B).await?,
		"orgBUid": ORG_B,
	}))
}

/// A real access token for the seeded account, minted against the app's own signing key — the
/// middleware verifies it exactly as it verifies a browser's.
async fn token(app: &App, org: &str) -> ClResult<String> {
	let key = app.secrets.get_or_create(saas_core::auth_mw::JWT_SECRET_KEY, 32).await?;
	let claims = saas_core::auth_mw::Claims {
		sub: ACCOUNT.to_owned(),
		org: Some(org.to_owned()),
		rol: Some("OWNER".to_owned()),
		opr: false,
		ep: 0,
		auth_at: Some(saas_core::types::Timestamp::now().0),
		imp: None,
		typ: None,
		iat: 0,
		exp: i64::from(u32::MAX),
	};
	jsonwebtoken::encode(
		&jsonwebtoken::Header::default(),
		&claims,
		&jsonwebtoken::EncodingKey::from_secret(&key),
	)
	.map_err(|e| Error::internal(format!("minting the test token: {e}")))
}

// vim: ts=4
