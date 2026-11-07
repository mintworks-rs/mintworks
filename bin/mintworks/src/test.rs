// SPDX-License-Identifier: MPL-2.0
//! `mintworks test <app-dir>` — the Rune `#[test]` runner.
//!
//! Every case gets its own real file database, so one case's rows are never another's fixture.
//! The bundle is compiled once, by the discovery build; everything else is per case: the
//! application is composed, migrated and seeded from scratch each time, and the router the case
//! drives is the one `AppBuilder::into_service` composes — the same stack, rate limiter included,
//! that production serves. Up to `MINTWORKS_TEST_JOBS` cases run at once, interleaved on the
//! caller's task; output stays in discovery order.
//!
//! With `PG_TEST_URL` set (and the `postgres` feature), a case's framework and app databases are
//! two databases on that server instead, created before the case and dropped after it: the
//! framework one a `CREATE DATABASE … TEMPLATE` clone of the database the discovery build
//! migrated once, the app one empty and owned by the run's single app role.

use std::{
	path::Path,
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering},
	},
};

use futures_util::{StreamExt, stream};

use mintworks_core::{App, ClResult, config::Config, error::Error};
use mintworks_script::testing::{self, Harness, Suite, TestFn};
#[cfg(feature = "postgres")]
use mintworks_store_postgres::sqlx::{AssertSqlSafe, Connection, PgConnection};
use serde_json::{Value as Json, json};

use crate::{
	Ext,
	app::{self, DbUrls, FrameworkStore},
};

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
	/// The counter is in the path: the slug alone maps `a.b` and `a-b` to one dir, cases run at
	/// once, and a process may hold several runs (`native.rs`), each with its own `discover`.
	fn new(name: &str) -> Self {
		static NEXT: AtomicUsize = AtomicUsize::new(0);
		let n = NEXT.fetch_add(1, Ordering::Relaxed);
		let slug: String =
			name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
		let dir =
			std::env::temp_dir().join(format!("mintworks-test-{}-{n}-{slug}", std::process::id()));
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

/// The run's state on the `PG_TEST_URL` server, dropped with the value after every [`PgDbs`]:
/// the framework template every case clones, and the one `NOSUPERUSER` role owning every app DB
/// (`PgAppDb` refuses a superuser; a clone keeps its template's owners, so not a role per case).
///
/// The template is the discovery build's database, which `open()` migrated and no `on_init` ever
/// touched: app init (a Rune `on_init`, the job and operator seeds) is not idempotent, so a clone
/// of an initialised database would run it twice. The app DB is therefore empty and not cloned.
#[cfg(feature = "postgres")]
struct PgRun {
	admin: String,
	/// `mintworks_{pid}_{tag}_`: unique per run, so concurrent runs never touch each other's.
	prefix: String,
	role: String,
	password: String,
	template: String,
	/// PostgreSQL 15+'s default `WAL_LOG` writes the whole template to WAL; `FILE_COPY` does not.
	strategy: &'static str,
}

/// Without the `postgres` feature there is never a run on a server.
#[cfg(not(feature = "postgres"))]
type PgRun = std::convert::Infallible;

#[cfg(feature = "postgres")]
impl PgRun {
	/// `None` when `PG_TEST_URL` is unset: the cases run on temp files, as without the feature.
	async fn new() -> ClResult<Option<Self>> {
		let Some(admin) = std::env::var("PG_TEST_URL").ok().filter(|v| !v.trim().is_empty()) else {
			return Ok(None);
		};
		let mut conn = PgConnection::connect(&admin).await.map_err(pg_fail)?;
		let (password, tag, version): (String, String, i32) = sqlx::query_as(
			"SELECT gen_random_uuid()::text, left(md5(gen_random_uuid()::text), 8),
			        current_setting('server_version_num')::int",
		)
		.fetch_one(&mut conn)
		.await
		.map_err(pg_fail)?;
		// A killed run leaks its databases and role; drop `mintworks_*` by hand.
		let prefix = format!("mintworks_{}_{tag}_", std::process::id());
		let run = Self {
			admin,
			role: format!("{prefix}app"),
			password,
			template: format!("{prefix}tpl_core"),
			prefix,
			strategy: if version >= 150_000 { " STRATEGY = FILE_COPY" } else { "" },
		};
		for sql in [
			format!("CREATE ROLE {} NOSUPERUSER LOGIN PASSWORD '{}'", run.role, run.password),
			format!("CREATE DATABASE {}", run.template),
		] {
			pg_exec(&mut conn, sql).await?;
		}
		Ok(Some(run))
	}

	/// A case's two databases. Case 0 is the discovery build: its framework database *is* the
	/// template, migrated in place and kept.
	async fn dbs(&self, case: usize) -> ClResult<PgDbs> {
		let [core, app] = ["core", "app"].map(|k| format!("{}{case}_{k}", self.prefix));
		let mut sql = vec![format!("CREATE DATABASE {app} OWNER {}", self.role)];
		let (core, names) = if case == 0 {
			(self.template.clone(), vec![app.clone()])
		} else {
			sql.insert(
				0,
				format!("CREATE DATABASE {core} TEMPLATE {}{}", self.template, self.strategy),
			);
			(core.clone(), vec![core, app.clone()])
		};
		let urls = case_urls(&self.admin, &core, &app, &self.role, &self.password)?;
		let dbs = PgDbs { admin: self.admin.clone(), names, urls };
		let mut conn = PgConnection::connect(&self.admin).await.map_err(pg_fail)?;
		for s in sql {
			pg_exec(&mut conn, s).await?;
		}
		Ok(dbs)
	}

	/// Ends every session left on the template: `CREATE DATABASE … TEMPLATE` refuses one in use,
	/// and a dropped pool closes its connections asynchronously.
	async fn seal(&self) -> ClResult<()> {
		let mut conn = PgConnection::connect(&self.admin).await.map_err(pg_fail)?;
		sqlx::query(
			"SELECT pg_terminate_backend(pid) FROM pg_stat_activity
			  WHERE datname = $1 AND pid <> pg_backend_pid()",
		)
		.bind(&self.template)
		.execute(&mut conn)
		.await
		.map_err(pg_fail)?;
		Ok(())
	}
}

#[cfg(feature = "postgres")]
impl Drop for PgRun {
	fn drop(&mut self) {
		drop_blocking(
			self.admin.clone(),
			vec![
				format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.template),
				format!("DROP ROLE IF EXISTS {}", self.role),
			],
		);
	}
}

/// A case's PostgreSQL databases, dropped with the value.
#[cfg(feature = "postgres")]
struct PgDbs {
	admin: String,
	names: Vec<String>,
	urls: DbUrls,
}

#[cfg(feature = "postgres")]
impl Drop for PgDbs {
	fn drop(&mut self) {
		let drops = self.names.iter().map(|n| format!("DROP DATABASE IF EXISTS {n} WITH (FORCE)"));
		drop_blocking(self.admin.clone(), drops.collect());
	}
}

#[cfg(feature = "postgres")]
#[allow(clippy::needless_pass_by_value)] // by value: it is a `map_err` callback
fn pg_fail(e: sqlx::Error) -> Error {
	Error::internal(format!("PG_TEST_URL: {e}"))
}

#[cfg(feature = "postgres")]
async fn pg_exec(conn: &mut PgConnection, sql: String) -> ClResult<()> {
	sqlx::raw_sql(AssertSqlSafe(sql)).execute(conn).await.map(drop).map_err(pg_fail)
}

/// Runs `sqls` on the admin connection, logging rather than raising: it backs a `Drop`.
#[cfg(feature = "postgres")]
fn drop_blocking(admin: String, sqls: Vec<String>) {
	// Its own runtime: `Drop` cannot await, and `FORCE` ends the case's pooled connections.
	let _ = std::thread::spawn(move || {
		let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
			return;
		};
		rt.block_on(async {
			let Ok(mut conn) = PgConnection::connect(&admin).await else { return };
			for sql in sqls {
				if let Err(e) = sqlx::raw_sql(AssertSqlSafe(sql.clone())).execute(&mut conn).await {
					eprintln!("{sql}: {e}");
				}
			}
		});
	})
	.join();
}

/// `admin` re-pointed at the core database, and at the app database under the run's app role —
/// parsed, so a socket URL's `?user=` cannot override the app role.
#[cfg(feature = "postgres")]
fn case_urls(admin: &str, core: &str, app: &str, role: &str, password: &str) -> ClResult<DbUrls> {
	use mintworks_store_postgres::sqlx::{ConnectOptions, postgres::PgConnectOptions};
	// Per session, so it needs no server access: a crash loses only a throwaway database.
	let opts = app::pg_options(admin)?.options([("synchronous_commit", "off")]);
	if opts.get_database().is_none() {
		return Err(Error::internal("PG_TEST_URL must name a database"));
	}
	// `to_url_lossy` drops `options`, so it is put back by hand.
	let url = |o: PgConnectOptions| {
		let mut url = o.to_url_lossy();
		if let Some(v) = o.get_options() {
			url.query_pairs_mut().append_pair("options", v);
		}
		url.to_string()
	};
	Ok(DbUrls {
		db: url(opts.clone().database(core)),
		app_db: url(opts.username(role).password(password).database(app)),
	})
}

/// One case's databases: temp files always (the data dir), plus PostgreSQL under `PG_TEST_URL`.
struct CaseDb {
	tmp: TmpDb,
	#[cfg(feature = "postgres")]
	pg: Option<PgDbs>,
}

impl CaseDb {
	#[cfg_attr(not(feature = "postgres"), allow(clippy::unused_async))]
	async fn new(name: &str, case: usize, pg: Option<&PgRun>) -> ClResult<Self> {
		let _ = (&pg, case);
		Ok(Self {
			tmp: TmpDb::new(name),
			#[cfg(feature = "postgres")]
			pg: match pg {
				Some(run) => Some(run.dbs(case).await?),
				None => None,
			},
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
pub async fn run(dir: &Path, filter: Option<&str>, ext: &Ext) -> ClResult<bool> {
	// Declared first so it drops last, after every case's databases it owns or templates.
	#[cfg(feature = "postgres")]
	let pg = PgRun::new().await?;
	#[cfg(not(feature = "postgres"))]
	let pg: Option<PgRun> = None;
	// Compiles the bundle and lists the cases; on PostgreSQL its database is every case's template.
	// Lives to the end of the run: an `fs::` call outside a case resolves against its directory.
	let probe = CaseDb::new("discover", 0, pg.as_ref()).await?;
	let (_, suite, _) = app::build_tests(dir, probe.tmp.config(), probe.urls(), ext, None).await?;
	let selected: Vec<&TestFn> = suite
		.tests
		.iter()
		.filter(|t| filter.is_none_or(|f| t.name.contains(f)))
		.collect();
	#[cfg(feature = "postgres")]
	if let Some(pg) = &pg {
		pg.seal().await?;
	}
	if let (Some(f), true) = (filter, selected.is_empty()) {
		println!("no test matches `{f}`");
		return Ok(false);
	}

	// Polled on this task rather than spawned: the build future holds `rune::Value`, not `Send`.
	// One thread polls every case, so the overlap is I/O, not CPU (Rune running a case);
	// and `buffered` yields in discovery order, so a slow case holds back later starts.
	let mut cases = stream::iter(selected.iter().enumerate())
		.map(|(i, &test)| run_one(dir, test, i + 1, pg.as_ref(), ext, &suite))
		.buffered(jobs());
	let (mut passed, mut failed) = (0_usize, 0_usize);
	while let Some(r) = cases.next().await {
		let (ok, line) = r?;
		if ok {
			passed += 1;
		} else {
			failed += 1;
		}
		println!("{line}");
	}

	println!("\n{passed} passed, {failed} failed");
	Ok(failed == 0)
}

/// `MINTWORKS_TEST_JOBS`, else half of `min(cores, 8)`; at least 1, and `1` runs one case at a
/// time.
fn jobs() -> usize {
	let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
	jobs_from(std::env::var("MINTWORKS_TEST_JOBS").ok().as_deref(), cores)
}

fn jobs_from(raw: Option<&str>, cores: usize) -> usize {
	raw.and_then(|v| v.trim().parse().ok()).unwrap_or(cores.min(8) / 2).max(1)
}

/// One case on its own databases: whether it passed, and its output.
async fn run_one(
	dir: &Path,
	test: &TestFn,
	case: usize,
	pg: Option<&PgRun>,
	ext: &Ext,
	suite: &Suite,
) -> ClResult<(bool, String)> {
	let db = CaseDb::new(&test.name, case, pg).await?;
	let out = run_case(dir, test, &db, ext, suite).await;
	// `Drop` blocks on the database drops; off this task, so the other cases keep running.
	let _ = tokio::task::spawn_blocking(move || drop(db)).await;
	out
}

async fn run_case(
	dir: &Path,
	test: &TestFn,
	db: &CaseDb,
	ext: &Ext,
	suite: &Suite,
) -> ClResult<(bool, String)> {
	let (builder, case, store) =
		app::build_tests(dir, db.tmp.config(), db.urls(), ext, Some(suite)).await?;

	let (app, router) = builder.into_service().await?;
	let session = seed(&app, store.as_ref()).await?;
	let harness = Arc::new(Harness { router, app: app.clone(), session });

	let name = &test.name;
	let out = match testing::run_test(&case.script, test, harness).await {
		Ok(()) => (true, format!("ok   {name}")),
		Err(e) => {
			let (status, code) = e.parts();
			(false, format!("FAIL {name}\n       {status} {code}: {e}"))
		}
	};
	app.shutdown_jobs().await;
	Ok(out)
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
	] {
		store.execute(sql, None).await?;
	}
	// Every `app.consent` kind gets a document too, and the required ones a consent, or the gate
	// and `test::signup` refuse the case. A document only where `<app-dir>/legal/*.md` published
	// none.
	let app_kinds = mintworks_auth::consent::ConsentKinds::all(app);
	let gating = [mintworks_auth::LegalKind::Tos, mintworks_auth::LegalKind::Privacy];
	for kind in gating.iter().chain(app_kinds.iter().map(|(k, _)| k)) {
		let sql = "INSERT INTO legal_docs
		   (kind, locale, version, title, body, sha256, effective_from, created_at)
		 SELECT $1, (SELECT locale FROM accounts WHERE uid = 'acc_01JCZ5X8K9N7QW3M6R2T4V8Y0A'),
		        'v1', 't', 'b', 'deadbeef', 0, 0
		  WHERE NOT EXISTS (SELECT 1 FROM legal_docs d WHERE d.kind = $1)";
		store.execute(sql, Some(kind.as_str())).await?;
	}
	// The consent names whichever document `current_legal_doc` picks (exact locale, else
	// newest): a consent against any other row still gates.
	let required = app_kinds.iter().filter(|(_, d)| d.required).map(|(k, _)| k);
	for kind in gating.iter().chain(required) {
		let sql = "INSERT INTO consents
		   (account_id, kind, legal_doc_id, doc_version, doc_sha256, granted, at)
		 SELECT a.id, d.kind, d.id, d.version, d.sha256, 1, 0
		   FROM accounts a, legal_docs d
		  WHERE a.uid = 'acc_01JCZ5X8K9N7QW3M6R2T4V8Y0A'
		    AND d.id = (SELECT p.id FROM legal_docs p WHERE p.kind = d.kind
		                 ORDER BY (p.locale = (SELECT locale FROM accounts
		                                        WHERE uid = 'acc_01JCZ5X8K9N7QW3M6R2T4V8Y0A')) DESC,
		                          p.effective_from DESC, p.id DESC
		                 LIMIT 1)
		    AND d.kind = $1";
		store.execute(sql, Some(kind.as_str())).await?;
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

#[cfg(test)]
mod jobs_tests {
	use super::jobs_from;

	#[test]
	fn the_job_count_parses_and_never_reaches_zero() {
		for (raw, cores, want) in [
			(Some("3"), 16, 3),
			(Some(" 2 "), 16, 2),
			(Some("0"), 16, 1),
			(Some("x"), 16, 4),
			(None, 1, 1),
			(None, 32, 4),
		] {
			assert_eq!(jobs_from(raw, cores), want, "{raw:?} on {cores} cores");
		}
	}
}

#[cfg(test)]
mod declaration_tests {
	use super::*;

	/// Every case re-runs `main` on one shared cell, so a `main` that saw the value after the
	/// first case would declare it twice here and fail that case's boot.
	#[tokio::test]
	async fn main_never_sees_a_test_env_value() {
		const MAIN: &str = r#"
pub fn main(app) {
	app.test_default("email.from", "noreply@decl.test");
	app.test_default("email.smtp.host", "localhost");
	app.test_env("APP_SEEN", "x");
	match env::get("APP_SEEN")? {
		() => (),
		_ => app.test_env("APP_SEEN", "declared twice"),
	}
}
"#;
		const TESTS: &str = r#"
#[test]
pub async fn first() {
	assert_eq!(env::get("APP_SEEN")?, "x");
	Ok(())
}

#[test]
pub async fn second() {
	assert_eq!(env::get("APP_SEEN")?, "x");
	Ok(())
}
"#;
		let dir = std::env::temp_dir().join(format!("mintworks-decl-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		std::fs::write(dir.join("main.rn"), MAIN).unwrap();
		std::fs::write(dir.join("tests.rn"), TESTS).unwrap();
		let passed = run(&dir, None, &Ext::default()).await.unwrap();
		let _ = std::fs::remove_dir_all(&dir);
		assert!(passed);
	}
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
			let urls = case_urls(admin, "c", "a", "r", "pw").unwrap();
			let core = app::pg_options(&urls.db).unwrap();
			let app_db = app::pg_options(&urls.app_db).unwrap();
			assert_eq!(core.get_database(), Some("c"), "{admin}");
			assert_eq!((app_db.get_username(), app_db.get_database()), ("r", Some("a")), "{admin}");
			assert_eq!(core.get_host(), app_db.get_host(), "{admin}");
			for o in [&core, &app_db] {
				let opts = o.get_options().unwrap_or_default();
				assert!(opts.contains("synchronous_commit=off"), "{admin}: {opts}");
			}
		}
		let err = case_urls("postgres://u:p@h", "c", "a", "r", "pw").map(drop).unwrap_err();
		assert!(err.to_string().contains("must name a database"), "{err}");
	}
}

// vim: ts=4
