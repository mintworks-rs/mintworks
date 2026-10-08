// SPDX-License-Identifier: MPL-2.0
//! `mintworks test <app-dir>` — the Rune `#[test]` runner.
//!
//! Every case gets its own real file database, so one case's rows are never another's fixture.
//! That means one build per case: the application is composed, migrated and seeded from scratch
//! each time, and the router the case drives is the one `AppBuilder::into_service` composes —
//! the same stack, rate limiter included, that production serves.
//!
//! With `PG_TEST_URL` set (and the `postgres` feature), a case's framework and app databases are
//! two fresh databases on that server instead, created before the case and dropped after it.

use std::{path::Path, sync::Arc};

use mintworks_core::{App, ClResult, config::Config, error::Error};
use mintworks_script::testing::{self, Harness};
use serde_json::{Value as Json, json};

use crate::app::{self, DbUrls, FrameworkStore};

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
		let dir =
			std::env::temp_dir().join(format!("mintworks-test-{}-{slug}", std::process::id()));
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

/// A case's two PostgreSQL databases on the `PG_TEST_URL` server, dropped with the value. The
/// app DB is owned by a `NOSUPERUSER` role of the same name, since `PgAppDb` refuses a superuser.
#[cfg(feature = "postgres")]
struct PgDbs {
	admin: String,
	names: [String; 2],
	urls: DbUrls,
}

#[cfg(feature = "postgres")]
impl PgDbs {
	/// `None` when `PG_TEST_URL` is unset: the case runs on temp files, as without the feature.
	async fn new(case: usize) -> ClResult<Option<Self>> {
		use mintworks_store_postgres::sqlx::{AssertSqlSafe, Connection, PgConnection};
		let Some(admin) = std::env::var("PG_TEST_URL").ok().filter(|v| !v.trim().is_empty()) else {
			return Ok(None);
		};
		let fail = |e: sqlx::Error| Error::internal(format!("PG_TEST_URL: {e}"));
		let names = ["core", "app"].map(|k| format!("mintworks_{}_{case}_{k}", std::process::id()));
		let [core, app] = &names;
		let mut conn = PgConnection::connect(&admin).await.map_err(fail)?;
		let password: String = sqlx::query_scalar("SELECT gen_random_uuid()::text")
			.fetch_one(&mut conn)
			.await
			.map_err(fail)?;
		let urls = case_urls(&admin, core, app, &password)?;
		for sql in [
			format!("DROP DATABASE IF EXISTS {core} WITH (FORCE)"),
			format!("DROP DATABASE IF EXISTS {app} WITH (FORCE)"),
			format!("DROP ROLE IF EXISTS {app}"),
			format!("CREATE ROLE {app} NOSUPERUSER LOGIN PASSWORD '{password}'"),
			format!("CREATE DATABASE {core}"),
			format!("CREATE DATABASE {app} OWNER {app}"),
		] {
			sqlx::raw_sql(AssertSqlSafe(sql)).execute(&mut conn).await.map_err(fail)?;
		}
		Ok(Some(Self { admin, names, urls }))
	}
}

/// `admin` re-pointed at the core database, and at the app database under its own role — parsed,
/// so a socket URL's `?user=` cannot override the app role.
#[cfg(feature = "postgres")]
fn case_urls(admin: &str, core: &str, app: &str, password: &str) -> ClResult<DbUrls> {
	use mintworks_store_postgres::sqlx::ConnectOptions;
	let opts = app::pg_options(admin)?;
	if opts.get_database().is_none() {
		return Err(Error::internal("PG_TEST_URL must name a database"));
	}
	Ok(DbUrls {
		db: opts.clone().database(core).to_url_lossy().to_string(),
		app_db: opts.username(app).password(password).database(app).to_url_lossy().to_string(),
	})
}

#[cfg(feature = "postgres")]
impl Drop for PgDbs {
	fn drop(&mut self) {
		use mintworks_store_postgres::sqlx::{AssertSqlSafe, Connection, PgConnection};
		let (admin, names) = (self.admin.clone(), self.names.clone());
		// Its own runtime: `Drop` cannot await, and `FORCE` ends the case's pooled connections.
		let _ = std::thread::spawn(move || {
			let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
				return;
			};
			rt.block_on(async {
				let Ok(mut conn) = PgConnection::connect(&admin).await else { return };
				let role = format!("DROP ROLE IF EXISTS {}", names[1]);
				let drops = names.map(|n| format!("DROP DATABASE IF EXISTS {n} WITH (FORCE)"));
				for sql in drops.into_iter().chain([role]) {
					if let Err(e) =
						sqlx::raw_sql(AssertSqlSafe(sql.clone())).execute(&mut conn).await
					{
						eprintln!("{sql}: {e}");
					}
				}
			});
		})
		.join();
	}
}

/// One case's databases: temp files always (the data dir), plus PostgreSQL under `PG_TEST_URL`.
struct CaseDb {
	tmp: TmpDb,
	#[cfg(feature = "postgres")]
	pg: Option<PgDbs>,
}

impl CaseDb {
	#[cfg_attr(not(feature = "postgres"), allow(clippy::unused_async))]
	async fn new(name: &str, case: usize) -> ClResult<Self> {
		let _ = case;
		Ok(Self {
			tmp: TmpDb::new(name),
			#[cfg(feature = "postgres")]
			pg: PgDbs::new(case).await?,
		})
	}

	#[cfg_attr(not(feature = "postgres"), allow(clippy::unused_self))]
	fn urls(&self) -> Option<&DbUrls> {
		#[cfg(feature = "postgres")]
		return self.pg.as_ref().map(|p| &p.urls);
		#[cfg(not(feature = "postgres"))]
		None
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
	let probe = CaseDb::new("discover", 0).await?;
	let (_, _, found, _) = app::build_tests(dir, probe.tmp.config(), probe.urls()).await?;
	let names: Vec<String> = found
		.into_iter()
		.map(|t| t.name)
		.filter(|n| filter.is_none_or(|f| n.contains(f)))
		.collect();
	drop(probe);

	let (mut passed, mut failed) = (0_usize, 0_usize);
	for (i, name) in names.iter().enumerate() {
		let db = CaseDb::new(name, i + 1).await?;
		let (builder, script, tests, store) =
			app::build_tests(dir, db.tmp.config(), db.urls()).await?;
		let Some(test) = tests.iter().find(|t| &t.name == name) else { continue };

		let (app, router) = builder.into_service().await?;
		let session = seed(&app, store.as_ref()).await?;
		let harness = Arc::new(Harness { router, app: app.clone(), session });

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
/// columns; `examples/booking/app-rust/tests/flow.rs` seeds the same shape the same way.
async fn seed(app: &App, store: &dyn FrameworkStore) -> ClResult<Json> {
	// Static SQL valid on both dialects, ids found by uid: a literal `accounts.id` would leave a
	// PostgreSQL identity sequence behind the row, and the next registration would collide.
	for sql in [
		"INSERT INTO accounts (uid, email, status, created_at)
		 VALUES ('acc_01JCZ5X8K9N7QW3M6R2T4V8Y0A', 'test@example.test', 'ACTIVE', 0)",
		"INSERT INTO orgs (uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES ('org_01JCZ5X8K9N7QW3M6R2T4V8Y0C', (SELECT id FROM orgs WHERE kind = 'ROOT'),
		         'SHARED', 'Test',
		         (SELECT id FROM accounts WHERE uid = 'acc_01JCZ5X8K9N7QW3M6R2T4V8Y0A'), 0)",
		"INSERT INTO orgs (uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES ('org_01JCZ5X8K9N7QW3M6R2T4V8Y0D', (SELECT id FROM orgs WHERE kind = 'ROOT'),
		         'SHARED', 'Other',
		         (SELECT id FROM accounts WHERE uid = 'acc_01JCZ5X8K9N7QW3M6R2T4V8Y0A'), 0)",
		// The root row: seller and catalogue writes gate on the *resolved seller's* org
		// (`require_seller_role`), a script `on_init` can only seed the root's, and `org_role`
		// walks ancestors, never down. Without it every write from `test::session()` is 403.
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 SELECT o.id, a.id, 'OWNER', 0, 0 FROM orgs o, accounts a
		  WHERE (o.uid IN ('org_01JCZ5X8K9N7QW3M6R2T4V8Y0C', 'org_01JCZ5X8K9N7QW3M6R2T4V8Y0D')
		         OR o.kind = 'ROOT')
		    AND a.uid = 'acc_01JCZ5X8K9N7QW3M6R2T4V8Y0A'",
		// No literal ids, and a document only where `<app-dir>/legal/*.md` published none. The
		// consent names whichever document `current_legal_doc` picks (exact locale, else newest):
		// a consent against any other row still gates.
		"INSERT INTO legal_docs
		   (kind, locale, version, title, body, sha256, effective_from, created_at)
		 SELECT k.kind, (SELECT locale FROM accounts WHERE uid = 'acc_01JCZ5X8K9N7QW3M6R2T4V8Y0A'),
		        'v1', 't', 'b', 'deadbeef', 0, 0
		   FROM (SELECT 'TOS' AS kind UNION ALL SELECT 'PRIVACY') k
		  WHERE NOT EXISTS (SELECT 1 FROM legal_docs d WHERE d.kind = k.kind)",
		"INSERT INTO consents (account_id, kind, legal_doc_id, doc_version, doc_sha256, granted, at)
		 SELECT a.id, d.kind, d.id, d.version, d.sha256, 1, 0
		   FROM accounts a, legal_docs d
		  WHERE a.uid = 'acc_01JCZ5X8K9N7QW3M6R2T4V8Y0A'
		    AND d.id = (SELECT p.id FROM legal_docs p WHERE p.kind = d.kind
		                 ORDER BY (p.locale = (SELECT locale FROM accounts
		                                        WHERE uid = 'acc_01JCZ5X8K9N7QW3M6R2T4V8Y0A')) DESC,
		                          p.effective_from DESC, p.id DESC
		                 LIMIT 1)
		    AND d.kind IN ('TOS', 'PRIVACY')",
	] {
		store.execute(sql).await?;
	}

	let root = app.store.root_org_id().await?;
	let root_uid = store
		.org_by_id(root)
		.await?
		.ok_or_else(|| Error::internal("seeding the test database: no root org"))?
		.uid;
	Ok(json!({
		"token": token(app, ORG).await?,
		"accountUid": ACCOUNT,
		"orgUid": ORG,
		"tokenB": token(app, ORG_B).await?,
		"orgBUid": ORG_B,
		// The account owns the root too: a case acting as the operator or seller uses this.
		"rootToken": token(app, root_uid.as_str()).await?,
	}))
}

/// A real access token for the seeded account, minted against the app's own signing key — the
/// middleware verifies it exactly as it verifies a browser's.
async fn token(app: &App, org: &str) -> ClResult<String> {
	let key = app.secrets.get_or_create(mintworks_core::auth_mw::JWT_SECRET_KEY, 32).await?;
	let claims = mintworks_core::auth_mw::Claims {
		sub: ACCOUNT.to_owned(),
		org: Some(org.to_owned()),
		rol: Some("OWNER".to_owned()),
		opr: false,
		ep: 0,
		auth_at: Some(mintworks_core::types::Timestamp::now().0),
		ses: None,
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

#[cfg(all(test, feature = "postgres"))]
mod tests {
	use super::*;

	#[test]
	fn case_urls_put_the_app_db_under_its_own_role() {
		for admin in [
			"postgres://u:p@h/db?sslmode=disable",
			"postgres:///db?host=/run/postgresql&user=x",
		] {
			let urls = case_urls(admin, "c", "a", "pw").unwrap();
			let core = app::pg_options(&urls.db).unwrap();
			let app_db = app::pg_options(&urls.app_db).unwrap();
			assert_eq!(core.get_database(), Some("c"), "{admin}");
			assert_eq!((app_db.get_username(), app_db.get_database()), ("a", Some("a")), "{admin}");
			assert_eq!(core.get_host(), app_db.get_host(), "{admin}");
		}
		let err = case_urls("postgres://u:p@h", "c", "a", "pw").map(drop).unwrap_err();
		assert!(err.to_string().contains("must name a database"), "{err}");
	}
}

// vim: ts=4
