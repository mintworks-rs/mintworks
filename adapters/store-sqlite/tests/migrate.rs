//! The migration runner itself, and the one invariant `schema.rs` and `migrations.rs` are
//! required to keep between them: a database upgraded from the oldest version this build still
//! upgrades from — the one `tests/fixtures/schema_v12.sql` records — reaches the same shape a
//! fresh install creates.
//!
//! The engine tests below drive synthetic [`Module`]s, never the framework's DDL, so a new
//! schema version needs nothing here beyond moving that fixture forward with the floor.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicI64, Ordering};

use mintworks_core::config::Config;
use mintworks_core::store::CoreStore;
use mintworks_store_sqlite::{FRAMEWORK, Fut, Module, SqliteStore, schema};

/// A temp directory that takes the database with it. `sqlite::memory:` gives each
/// *connection* its own database, so the store needs a file.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir()
			.join(format!("mintworks-migrate-test-{}-{name}", std::process::id()));
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

async fn open(db: &TmpDb) -> SqliteStore {
	SqliteStore::open(&db.config()).await.unwrap()
}

// ---------------------------------------------------------------------------------------------
// Fresh install vs. upgrade
// ---------------------------------------------------------------------------------------------

/// Every column and index of every table, read out of SQLite's pragmas rather than diffed as
/// DDL text: `sqlite_master.sql` is stored as written, and `ALTER TABLE ADD COLUMN` appends in
/// a spelling `create` never produces, so a text comparison always false-positives.
///
/// CHECK constraints are not compared: no pragma exposes them, and a `sqlite_master.sql` text
/// diff would false-positive on spelling. Every database at or above the floor carries them
/// (`claude-docs/adapter-contract.md` §2), so this test would not catch a dropped one.
type Shape = std::collections::BTreeMap<String, (Vec<String>, Vec<String>)>;

async fn shape(pool: &sqlx::SqlitePool) -> Shape {
	use sqlx::Row;

	let tables: Vec<String> = sqlx::query_scalar(
		"SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
		 ORDER BY name",
	)
	.fetch_all(pool)
	.await
	.unwrap();

	let mut out = Shape::new();
	for table in tables {
		// Sorted, not in declaration order: `ALTER TABLE ADD COLUMN` can only append, so an
		// upgraded table holds the same columns in a different order and always will.
		let mut columns: Vec<String> =
			sqlx::query(sqlx::AssertSqlSafe(format!("PRAGMA table_info({table})")))
				.fetch_all(pool)
				.await
				.unwrap()
				.iter()
				.map(|r| {
					format!(
						"{} {} notnull={} default={:?} pk={}",
						r.get::<String, _>("name"),
						r.get::<String, _>("type"),
						r.get::<i64, _>("notnull"),
						r.get::<Option<String>, _>("dflt_value"),
						r.get::<i64, _>("pk"),
					)
				})
				.collect();
		columns.sort();

		let mut indexes = Vec::new();
		for row in sqlx::query(sqlx::AssertSqlSafe(format!("PRAGMA index_list({table})")))
			.fetch_all(pool)
			.await
			.unwrap()
		{
			let name: String = row.get("name");
			let cols: Vec<String> =
				sqlx::query(sqlx::AssertSqlSafe(format!("PRAGMA index_info({name})")))
					.fetch_all(pool)
					.await
					.unwrap()
					.iter()
					.map(|r| r.get::<Option<String>, _>("name").unwrap_or_default())
					.collect();
			indexes.push(format!(
				"{name} unique={} partial={} ({})",
				row.get::<i64, _>("unique"),
				row.get::<i64, _>("partial"),
				cols.join(", "),
			));
		}
		indexes.sort();
		out.insert(table, (columns, indexes));
	}
	out
}

/// A store on the version-12 fixture, stamped as v12 by the fixture itself.
async fn open_v12(db: &TmpDb) -> SqliteStore {
	let store = open(db).await;
	sqlx::raw_sql(include_str!("fixtures/schema_v12.sql"))
		.execute(store.write_pool())
		.await
		.unwrap();
	store
}

#[tokio::test]
async fn an_upgraded_database_reaches_the_same_shape_as_a_fresh_install() {
	let upgraded_db = TmpDb::new("upgraded");
	let upgraded = open_v12(&upgraded_db).await;
	upgraded.migrate(&[FRAMEWORK]).await.unwrap();

	let fresh_db = TmpDb::new("fresh");
	let fresh = open(&fresh_db).await;
	fresh.migrate(&[FRAMEWORK]).await.unwrap();

	// Every difference at once, not just the first: one missing `if from < N` block usually
	// leaves several tables behind, and the point of the message is to name all of them.
	let (a, b) = (shape(upgraded.read_pool()).await, shape(fresh.read_pool()).await);
	let mut diffs = Vec::new();
	for (table, fresh_shape) in &b {
		match a.get(table) {
			None => diffs.push(format!("table '{table}' is missing from the upgraded database")),
			Some(up) => {
				for (what, up, fresh) in
					[("columns", &up.0, &fresh_shape.0), ("indexes", &up.1, &fresh_shape.1)]
				{
					if up != fresh {
						let missing: Vec<_> = fresh.iter().filter(|c| !up.contains(c)).collect();
						let extra: Vec<_> = up.iter().filter(|c| !fresh.contains(c)).collect();
						diffs.push(format!(
							"{what} of '{table}' differ: missing {missing:?}, unexpected {extra:?}"
						));
					}
				}
			}
		}
	}
	for table in a.keys().filter(|t| !b.contains_key(*t)) {
		diffs.push(format!("table '{table}' exists only in the upgraded database"));
	}
	assert!(diffs.is_empty(), "upgraded != fresh install:\n  {}", diffs.join("\n  "));

	let version: i64 =
		sqlx::query_scalar("SELECT version FROM schema_version WHERE module = 'saas'")
			.fetch_one(upgraded.read_pool())
			.await
			.unwrap();
	assert_eq!(version, schema::VERSION);
}

/// v13 adds the object store and nothing else, so every row a v12 database already holds has to
/// come through the upgrade untouched.
#[tokio::test]
async fn the_v13_upgrade_adds_the_object_store_and_keeps_the_rows_that_were_there() {
	let db = TmpDb::new("v13-keeps-rows");
	let store = open_v12(&db).await;

	sqlx::raw_sql(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_a', 'a@e.st', 0);
		 INSERT INTO orgs (id, uid, kind, name, owner_account_id, created_at)
		   VALUES (1, 'org_root', 'ROOT', 'Platform', 1, 0);",
	)
	.execute(store.write_pool())
	.await
	.unwrap();

	store.migrate(&[FRAMEWORK]).await.unwrap();

	let (uid, email): (String, String) =
		sqlx::query_as("SELECT uid, email FROM accounts WHERE id = 1")
			.fetch_one(store.read_pool())
			.await
			.unwrap();
	assert_eq!((uid.as_str(), email.as_str()), ("acc_a", "a@e.st"));
	let root: String = sqlx::query_scalar("SELECT uid FROM orgs WHERE kind = 'ROOT'")
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	assert_eq!(root, "org_root", "the root org was not replaced");
	assert!(has_table(store.read_pool(), "objects").await);
	assert!(has_table(store.read_pool(), "object_index").await);
}

/// Every secret written before v15 was a global one, and it must still be found at org 0.
#[tokio::test]
async fn the_v15_upgrade_moves_every_secret_to_org_zero() {
	let db = TmpDb::new("v15-secrets");
	let store = open_v12(&db).await;
	sqlx::raw_sql(
		"INSERT INTO secrets (key, nonce, ciphertext, updated_at, updated_by)
		   VALUES ('auth.jwt_key', x'00', x'01', 7, NULL), ('pow.hmac_key', x'02', x'03', 8, 1);",
	)
	.execute(store.write_pool())
	.await
	.unwrap();

	store.migrate(&[FRAMEWORK]).await.unwrap();

	let rows: Vec<(i64, String, Vec<u8>, i64)> =
		sqlx::query_as("SELECT org_id, key, ciphertext, updated_at FROM secrets ORDER BY key")
			.fetch_all(store.read_pool())
			.await
			.unwrap();
	assert_eq!(
		rows,
		vec![
			(0, "auth.jwt_key".to_owned(), vec![1], 7),
			(0, "pow.hmac_key".to_owned(), vec![3], 8)
		]
	);
	assert!(store.secret_get(0, "auth.jwt_key").await.unwrap().is_some());
}

/// v17's payment terms are NULL — "inherit" — on every row that existed before it.
#[tokio::test]
async fn the_v17_upgrade_leaves_existing_payment_terms_null() {
	let db = TmpDb::new("v17-terms");
	let store = open_v12(&db).await;
	sqlx::raw_sql(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_a', 'a@e.st', 0);
		 INSERT INTO orgs (id, uid, kind, name, owner_account_id, created_at)
		   VALUES (1, 'org_root', 'ROOT', 'Platform', 1, 0);
		 INSERT INTO sellers (id, uid, org_id, nav_base_url, created_at)
		   VALUES (1, 'sel_a', 1, '', 0);
		 INSERT INTO billing_parties (id, uid, org_id, kind, name, country, created_at, updated_at)
		   VALUES (1, 'prt_a', 1, 'C', 'Vevo', 'HU', 0, 0);",
	)
	.execute(store.write_pool())
	.await
	.unwrap();

	store.migrate(&[FRAMEWORK]).await.unwrap();

	let seller: Option<i64> = sqlx::query_scalar("SELECT payment_days FROM sellers")
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	let party: (Option<i64>, Option<String>) =
		sqlx::query_as("SELECT payment_days, payment_method FROM billing_parties")
			.fetch_one(store.read_pool())
			.await
			.unwrap();
	assert_eq!((seller, party), (None, (None, None)));
}

/// v18 split the old `vat_scheme = 'KATA'` into a VAT scheme and an income regime.
#[tokio::test]
async fn the_v18_upgrade_moves_kata_to_the_income_regime() {
	let db = TmpDb::new("v18-kata");
	let store = open_v12(&db).await;
	sqlx::raw_sql(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_a', 'a@e.st', 0);
		 INSERT INTO orgs (id, uid, kind, name, owner_account_id, created_at)
		   VALUES (1, 'org_root', 'ROOT', 'Platform', 1, 0);
		 INSERT INTO sellers (id, uid, org_id, nav_base_url, created_at)
		   VALUES (1, 'sel_a', 1, '', 0);
		 INSERT INTO seller_versions (seller_ver, seller_id, status, name, tax_number, postcode,
		   city, street, vat_scheme, created_at, valid_from, superseded_at)
		   VALUES (1, 1, 'ARCHIVED', 'A', '12345678-1-42', '1111', 'Bp', 'U 1', 'KATA', 0, 1, 2),
		          (2, 1, 'CURRENT', 'A', '12345678-1-42', '1111', 'Bp', 'U 1', 'ALANYI_MENTES',
		           0, 2, NULL);",
	)
	.execute(store.write_pool())
	.await
	.unwrap();

	store.migrate(&[FRAMEWORK]).await.unwrap();

	let rows: Vec<(String, String)> =
		sqlx::query_as("SELECT vat_scheme, income_regime FROM seller_versions ORDER BY seller_ver")
			.fetch_all(store.read_pool())
			.await
			.unwrap();
	assert_eq!(
		rows,
		vec![
			("NORMAL".to_owned(), "KATA".to_owned()),
			("ALANYI_MENTES".to_owned(), "NONE".to_owned())
		]
	);
}

/// Below the floor there is no block left to run, so a stamp would claim a shape the database does
/// not have. Refuse the whole migration instead, exactly as the runner refuses a newer build.
#[tokio::test]
async fn a_database_below_the_floor_is_refused_rather_than_stamped_forward() {
	let db = TmpDb::new("below-floor");
	let store = open_v12(&db).await;
	sqlx::query("UPDATE schema_version SET version = 11 WHERE module = 'saas'")
		.execute(store.write_pool())
		.await
		.unwrap();

	let err = store.migrate(&[FRAMEWORK]).await.unwrap_err().to_string();
	assert!(err.contains("11") && err.contains("12"), "both versions must be named: {err}");

	let version: i64 =
		sqlx::query_scalar("SELECT version FROM schema_version WHERE module = 'saas'")
			.fetch_one(store.read_pool())
			.await
			.unwrap();
	assert_eq!(version, 11, "a refused migration must not stamp");
	assert!(!has_table(store.read_pool(), "objects").await, "and must not have run its DDL");
}

// ---------------------------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------------------------

/// Where the synthetic `toy` module records the `from` it was handed.
static SEEN: AtomicI64 = AtomicI64::new(-1);

async fn has_table(pool: &sqlx::SqlitePool, name: &str) -> bool {
	sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?")
		.bind(name)
		.fetch_one(pool)
		.await
		.unwrap()
		> 0
}

fn v1(conn: &mut sqlx::SqliteConnection, from: i64) -> Fut<'_> {
	Box::pin(async move {
		SEEN.store(from, Ordering::SeqCst);
		sqlx::raw_sql("CREATE TABLE toy (id INTEGER PRIMARY KEY)")
			.execute(conn)
			.await
			.map_err(|e| mintworks_core::error::Error::internal(e.to_string()))?;
		Ok(())
	})
}

fn v2(conn: &mut sqlx::SqliteConnection, from: i64) -> Fut<'_> {
	Box::pin(async move {
		SEEN.store(from, Ordering::SeqCst);
		sqlx::raw_sql("ALTER TABLE toy ADD COLUMN label TEXT")
			.execute(conn)
			.await
			.map_err(|e| mintworks_core::error::Error::internal(e.to_string()))?;
		Ok(())
	})
}

#[tokio::test]
async fn apply_is_handed_the_recorded_version_and_stamped_only_after_it_returns() {
	let db = TmpDb::new("from-handed-through");
	let store = open(&db).await;

	store.migrate(&[Module { name: "toy", version: 1, apply: v1 }]).await.unwrap();
	assert_eq!(SEEN.load(Ordering::SeqCst), 0, "a never-applied module must see from == 0");

	store.migrate(&[Module { name: "toy", version: 2, apply: v2 }]).await.unwrap();
	assert_eq!(SEEN.load(Ordering::SeqCst), 1, "an upgrade must see the recorded version");

	let version: i64 =
		sqlx::query_scalar("SELECT version FROM schema_version WHERE module = 'toy'")
			.fetch_one(store.read_pool())
			.await
			.unwrap();
	assert_eq!(version, 2);
}

static RUNS: AtomicI64 = AtomicI64::new(0);

fn counting(conn: &mut sqlx::SqliteConnection, _from: i64) -> Fut<'_> {
	Box::pin(async move {
		RUNS.fetch_add(1, Ordering::SeqCst);
		sqlx::raw_sql("CREATE TABLE counted (id INTEGER PRIMARY KEY)")
			.execute(conn)
			.await
			.map_err(|e| mintworks_core::error::Error::internal(e.to_string()))?;
		Ok(())
	})
}

#[tokio::test]
async fn a_module_already_at_its_version_is_skipped_rather_than_reapplied() {
	let db = TmpDb::new("skip-applied");
	let store = open(&db).await;
	let m = Module { name: "counted", version: 1, apply: counting };

	store.migrate(&[m]).await.unwrap();
	store.migrate(&[m]).await.unwrap();

	// A second run would hit `table counted already exists`; the count says it was not even tried.
	assert_eq!(RUNS.load(Ordering::SeqCst), 1);
}

fn dupe(conn: &mut sqlx::SqliteConnection, _from: i64) -> Fut<'_> {
	Box::pin(async move {
		sqlx::raw_sql("CREATE TABLE dupe (id INTEGER PRIMARY KEY)")
			.execute(conn)
			.await
			.map_err(|e| mintworks_core::error::Error::internal(e.to_string()))?;
		Ok(())
	})
}

#[tokio::test]
async fn a_duplicate_module_name_is_refused_rather_than_silently_skipped() {
	let db = TmpDb::new("duplicate-name");
	let store = open(&db).await;
	let m = Module { name: "dupe", version: 1, apply: dupe };

	let err = store.migrate(&[m, m]).await.unwrap_err().to_string();
	assert!(err.contains("dupe"), "the error must name the module: {err}");
	assert!(!has_table(store.read_pool(), "dupe").await, "neither pass may have run its DDL");
}

fn noop(_conn: &mut sqlx::SqliteConnection, _from: i64) -> Fut<'_> {
	Box::pin(async move { Ok(()) })
}

#[tokio::test]
async fn a_database_from_a_newer_build_is_refused() {
	let db = TmpDb::new("newer-build");
	let store = open(&db).await;
	store
		.migrate(&[Module { name: "ahead", version: 1, apply: noop }])
		.await
		.unwrap();
	sqlx::query("UPDATE schema_version SET version = 7 WHERE module = 'ahead'")
		.execute(store.write_pool())
		.await
		.unwrap();

	let err = store
		.migrate(&[Module { name: "ahead", version: 2, apply: noop }])
		.await
		.unwrap_err()
		.to_string();
	assert!(err.contains('7') && err.contains('2'), "both versions must be named: {err}");
}

fn dangling(conn: &mut sqlx::SqliteConnection, _from: i64) -> Fut<'_> {
	Box::pin(async move {
		sqlx::raw_sql(
			"CREATE TABLE parent (id INTEGER PRIMARY KEY);
			 CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent(id));
			 INSERT INTO child (id, parent_id) VALUES (1, 404);",
		)
		.execute(conn)
		.await
		.map_err(|e| mintworks_core::error::Error::internal(e.to_string()))?;
		Ok(())
	})
}

#[tokio::test]
async fn a_dangling_foreign_key_rolls_the_whole_migration_back() {
	let db = TmpDb::new("dangling-fk");
	let store = open(&db).await;

	let err = store
		.migrate(&[Module { name: "fk", version: 1, apply: dangling }])
		.await
		.unwrap_err()
		.to_string();
	assert!(err.contains("child"), "the error must name the offending table: {err}");
	assert!(!has_table(store.read_pool(), "child").await, "the transaction must have rolled back");
	// `schema_version` is created inside the same transaction, so its absence is the proof that
	// nothing was stamped.
	assert!(!has_table(store.read_pool(), "schema_version").await);
}

fn first(conn: &mut sqlx::SqliteConnection, _from: i64) -> Fut<'_> {
	Box::pin(async move {
		sqlx::raw_sql("CREATE TABLE ordered_first (id INTEGER PRIMARY KEY)")
			.execute(conn)
			.await
			.map_err(|e| mintworks_core::error::Error::internal(e.to_string()))?;
		Ok(())
	})
}

/// Legal only if `first` already ran in the same transaction.
fn second(conn: &mut sqlx::SqliteConnection, _from: i64) -> Fut<'_> {
	Box::pin(async move {
		sqlx::raw_sql(
			"CREATE VIEW ordered_view AS SELECT id FROM ordered_first;
			 SELECT * FROM ordered_view;",
		)
		.execute(conn)
		.await
		.map_err(|e| mintworks_core::error::Error::internal(e.to_string()))?;
		Ok(())
	})
}

fn boom(_conn: &mut sqlx::SqliteConnection, _from: i64) -> Fut<'_> {
	Box::pin(async move { Err(mintworks_core::error::Error::internal("boom")) })
}

#[tokio::test]
async fn modules_apply_in_list_order_inside_one_transaction() {
	let db = TmpDb::new("list-order");
	let store = open(&db).await;

	store
		.migrate(&[
			Module { name: "first", version: 1, apply: first },
			Module { name: "second", version: 1, apply: second },
		])
		.await
		.unwrap();
	assert!(has_table(store.read_pool(), "ordered_first").await);

	let rollback_db = TmpDb::new("list-order-rollback");
	let rollback = open(&rollback_db).await;
	rollback
		.migrate(&[
			Module { name: "first", version: 1, apply: first },
			Module { name: "boom", version: 1, apply: boom },
		])
		.await
		.unwrap_err();
	assert!(
		!has_table(rollback.read_pool(), "ordered_first").await,
		"a later module's failure must roll the earlier one back too"
	);
}

// vim: ts=4
