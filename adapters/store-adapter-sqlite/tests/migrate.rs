//! The migration runner itself, and the one invariant `schema.rs` and `migrations.rs` are
//! required to keep between them: a database upgraded from version 1 reaches the same shape a
//! fresh install creates.
//!
//! The engine tests below drive synthetic [`Module`]s, never the framework's DDL, so a new
//! schema version needs nothing here — the parity test walks `tests/fixtures/schema_v1.sql` up
//! to whatever [`schema::VERSION`] currently is.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicI64, Ordering};

use saas_core::config::Config;
use store_adapter_sqlite::{FRAMEWORK, Fut, Module, SqliteStore, schema};

/// A temp directory that takes the database with it. `sqlite::memory:` gives each
/// *connection* its own database, so the store needs a file.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir =
			std::env::temp_dir().join(format!("saas-migrate-test-{}-{name}", std::process::id()));
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
/// `PRAGMA table_info` does not expose CHECK constraints, which is deliberate here: `invoices`'
/// `CHECK (status = 'DRAFT' OR seller_ver IS NOT NULL)` is a fresh-install-only rule (SQLite
/// cannot `ALTER TABLE ADD CHECK`), so a `sqlite_master.sql` diff would flag an accepted
/// divergence documented in `claude-docs/adapter-contract.md`.
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

/// A store on the version-1 fixture, stamped as such but not yet migrated.
async fn open_v1(db: &TmpDb) -> SqliteStore {
	let store = open(db).await;
	sqlx::raw_sql(include_str!("fixtures/schema_v1.sql"))
		.execute(store.writer())
		.await
		.unwrap();
	// The hand-stamp recipe from `example/README.md`, verbatim, so this covers that too.
	sqlx::raw_sql(
		"DROP TABLE migrations;
		 CREATE TABLE schema_version (
			module     TEXT    NOT NULL PRIMARY KEY,
			version    INTEGER NOT NULL,
			updated_at INTEGER NOT NULL
		 );
		 INSERT INTO schema_version (module, version, updated_at)
			VALUES ('saas', 1, unixepoch());",
	)
	.execute(store.writer())
	.await
	.unwrap();
	store
}

#[tokio::test]
async fn an_upgraded_database_reaches_the_same_shape_as_a_fresh_install() {
	let upgraded_db = TmpDb::new("upgraded");
	let upgraded = open_v1(&upgraded_db).await;
	upgraded.migrate(&[FRAMEWORK]).await.unwrap();

	let fresh_db = TmpDb::new("fresh");
	let fresh = open(&fresh_db).await;
	fresh.migrate(&[FRAMEWORK]).await.unwrap();

	// Every difference at once, not just the first: one missing `if from < N` block usually
	// leaves several tables behind, and the point of the message is to name all of them.
	let (a, b) = (shape(upgraded.reader()).await, shape(fresh.reader()).await);
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
			.fetch_one(upgraded.reader())
			.await
			.unwrap();
	assert_eq!(version, schema::VERSION);
}

/// The parity test above walks from version 1, which runs the `CREATE` pass and so never takes
/// the rebuild branch — but a *shipped* database at 4 or 5 does, and that branch is a table
/// rebuild: a column-level `UNIQUE` leaves an implicit `sqlite_autoindex` that `DROP INDEX`
/// cannot remove, so the only way to widen `request_id` to `(tenant_id, request_id)` is to
/// recreate the table. The rows have to survive it, `payment_allocations` has to still point at
/// them, and the shape has to land where a fresh install lands.
///
/// Both versions, because `if from == 5` left a database stamped 4 with the global `UNIQUE`
/// forever: version 6 is never reapplied.
#[tokio::test]
async fn the_payments_rebuild_keeps_its_rows_and_reaches_the_fresh_shape() {
	// Version 4 is the same table without `redirect_url`; the `from == 4` block adds it back
	// before the rebuild, whose `INSERT … SELECT` reads it.
	rebuild_from(4, "").await;
	rebuild_from(5, "redirect_url	TEXT,").await;
}

async fn rebuild_from(version: i64, redirect_url: &str) {
	use sqlx::Row as _;

	let db = TmpDb::new(&format!("payments-v{version}"));
	let store = open(&db).await;
	store.migrate(&[FRAMEWORK]).await.unwrap();

	// Back to the pre-rebuild shape — a column-level `UNIQUE` on `request_id` — and re-stamped,
	// so `apply` is handed the `from` a shipped database hands it.
	sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
		"PRAGMA foreign_keys = OFF;
		 DROP TABLE payment_allocations;
		 DROP TABLE payments;
		 CREATE TABLE payments (
			id		INTEGER NOT NULL PRIMARY KEY,
			uid		TEXT NOT NULL UNIQUE,
			tenant_id	INTEGER NOT NULL REFERENCES tenants(id),
			kind		TEXT NOT NULL,
			provider	TEXT,
			provider_ref	TEXT,
			{redirect_url}
			request_id	TEXT UNIQUE,
			status		TEXT NOT NULL DEFAULT 'PENDING'
					CHECK (status IN ('PENDING','AWAITING_USER','RESERVED','AUTHORIZED',
					                  'SUCCEEDED','PARTIALLY_SUCCEEDED','FAILED','CANCELED',
					                  'EXPIRED','REFUNDED')),
			amount		INTEGER NOT NULL,
			currency	TEXT NOT NULL REFERENCES currencies(code),
			refunded_amount	INTEGER NOT NULL DEFAULT 0,
			received_at	INTEGER,
			ext_ref		TEXT,
			note		TEXT,
			created_by	INTEGER,
			created_at	INTEGER NOT NULL,
			updated_at	INTEGER NOT NULL,
			CHECK (refunded_amount >= 0 AND refunded_amount <= amount)
		 );
		 CREATE INDEX idx_payment_tenant ON payments(tenant_id, id DESC);
		 CREATE INDEX idx_payment_ext    ON payments(ext_ref) WHERE ext_ref IS NOT NULL;
		 CREATE UNIQUE INDEX idx_payment_provider_ref
			ON payments(provider, provider_ref) WHERE provider_ref IS NOT NULL;
		 CREATE TABLE payment_allocations (
			payment_id	INTEGER NOT NULL REFERENCES payments(id) ON DELETE CASCADE,
			invoice_id	INTEGER NOT NULL REFERENCES invoices(id),
			amount		INTEGER NOT NULL,
			allocated_at	INTEGER NOT NULL,
			allocated_by	INTEGER,
			PRIMARY KEY (payment_id, invoice_id)
		 ) WITHOUT ROWID;
		 CREATE INDEX idx_payment_allocation_invoice ON payment_allocations(invoice_id);
		 UPDATE schema_version SET version = {version} WHERE module = 'saas';"
	)))
	.execute(&mut *store.writer().acquire().await.unwrap())
	.await
	.unwrap();

	// A tenant, an invoice and a payment with an allocation against it, so the rebuild has rows
	// and a foreign key to carry across.
	sqlx::raw_sql(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_a', 'a@e.st', 0);
		 INSERT INTO tenants (id, uid, kind, name, owner_account_id, created_at)
		   VALUES (1, 'tnt_a', 'O', 'T', 1, 0);
		 INSERT INTO payments
		   (id, uid, tenant_id, kind, request_id, status, amount, currency, created_at, updated_at)
		   VALUES (42, 'pay_keep', 1, 'TRANSFER', 'sub-2026-01', 'SUCCEEDED', 1000, 'HUF', 7, 7);",
	)
	.execute(&mut *store.writer().acquire().await.unwrap())
	.await
	.unwrap();

	store.migrate(&[FRAMEWORK]).await.unwrap();

	// The row survived, id verbatim — which is what keeps `payment_allocations` pointing at it.
	let row = sqlx::query("SELECT * FROM payments WHERE id = 42")
		.fetch_one(store.reader())
		.await
		.unwrap();
	assert_eq!(row.get::<String, _>("uid"), "pay_keep");
	assert_eq!(row.get::<String, _>("request_id"), "sub-2026-01");
	assert_eq!(row.get::<i64, _>("amount"), 1000);
	assert_eq!(row.get::<i64, _>("created_at"), 7);

	// The key is per tenant now: a second tenant may spend the same one, and the same tenant
	// may not.
	sqlx::query(
		"INSERT INTO tenants (id, uid, kind, name, owner_account_id, created_at)
		 VALUES (2, 'tnt_b', 'O', 'M', 1, 0)",
	)
	.execute(store.writer())
	.await
	.unwrap();
	let insert = |tenant: i64, uid: &'static str| {
		sqlx::query(
			"INSERT INTO payments
			   (uid, tenant_id, kind, request_id, status, amount, currency, created_at, updated_at)
			   VALUES (?, ?, 'TRANSFER', 'sub-2026-01', 'SUCCEEDED', 1000, 'HUF', 8, 8)",
		)
		.bind(uid)
		.bind(tenant)
		.execute(store.writer())
	};
	insert(2, "pay_other").await.expect("another tenant's key is its own");
	insert(1, "pay_dupe").await.unwrap_err();

	// And the shape is where a fresh install lands, indexes included.
	let fresh_db = TmpDb::new(&format!("payments-v{version}-fresh"));
	let fresh = open(&fresh_db).await;
	fresh.migrate(&[FRAMEWORK]).await.unwrap();
	let (a, b) = (shape(store.reader()).await, shape(fresh.reader()).await);
	for table in ["payments", "payment_allocations"] {
		assert_eq!(a[table], b[table], "{table} differs from a fresh install");
	}
}

/// Version 7 widens `invoices.status` with `PENDING`, and SQLite cannot alter a CHECK — so the
/// widest table in the schema is rebuilt, under a live example database holding NAV invoice
/// numbers that must not be recreated. Everything keyed on `invoices.id` has to come through
/// it: lines, VAT groups, `payment_allocations`, `invoice_documents` and `nav_submissions`,
/// plus the partial unique index that is the only guard against a second storno.
#[tokio::test]
async fn the_invoices_rebuild_keeps_every_row_and_the_storno_guard() {
	use sqlx::Row as _;

	const V1: &str = include_str!("fixtures/schema_v1.sql");

	let db = TmpDb::new("invoices-v6");
	let store = open(&db).await;
	store.migrate(&[FRAMEWORK]).await.unwrap();

	// Back to the pre-rebuild shape and re-stamped, so `apply` is handed the `from` a shipped
	// database hands it. `Module::apply` runs every `if from < N` block it has, so stopping the
	// *migration* at 6 is not possible — the table is put back instead, from the version-1
	// fixture plus the one column version 2 appended to it, which is exactly version 6's shape.
	let start = V1.find("CREATE TABLE IF NOT EXISTS invoices (").unwrap();
	let v6_invoices = &V1[start..][..V1[start..].find("\n);").unwrap() + 3];
	sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
		"PRAGMA foreign_keys = OFF;
		 DROP TABLE invoices;
		 {v6_invoices}
		 ALTER TABLE invoices
		   ADD COLUMN seller_ver INTEGER REFERENCES seller_versions(seller_ver);
		 UPDATE schema_version SET version = 6 WHERE module = 'saas';"
	)))
	.execute(&mut *store.writer().acquire().await.unwrap())
	.await
	.unwrap();

	sqlx::raw_sql(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_a', 'a@e.st', 0);
		 INSERT INTO tenants (id, uid, kind, name, owner_account_id, created_at)
		   VALUES (1, 'tnt_a', 'O', 'T', 1, 0);
		 INSERT INTO sellers (id, nav_base_url, created_at) VALUES (1, 'https://x.invalid', 0);
		 INSERT INTO seller_versions (seller_ver, seller_id, status, name, country, tax_number,
		                              postcode, city, street, created_at, valid_from)
		   VALUES (1, 1, 'CURRENT', 'Teszt Kft.', 'HU', '12345678242',
		           '1011', 'Budapest', 'Fo utca 1.', 0, 0);
		 INSERT INTO invoices (id, uid, tenant_id, seller_id, seller_ver, kind, status, number,
		                       issued_at, fulfilment_date, buyer_name, currency, net, vat, gross,
		                       created_at, updated_at)
		   VALUES (10, 'inv_orig', 1, 1, 1, 'NORMAL', 'ISSUED', 'A2026/000001',
		           7, '2026-01-01', 'Vevo Bt.', 'HUF', 1000, 270, 1270, 7, 7);
		 INSERT INTO invoices (id, uid, tenant_id, seller_id, seller_ver, kind, status, number,
		                       issued_at, fulfilment_date, buyer_name, currency, net, vat, gross,
		                       original_invoice_id, created_at, updated_at)
		   VALUES (11, 'inv_storno', 1, 1, 1, 'STORNO', 'ISSUED', 'A2026/000002',
		           8, '2026-01-01', 'Vevo Bt.', 'HUF', -1000, -270, -1270, 10, 8, 8);
		 INSERT INTO invoice_lines (invoice_id, line_no, description, qty, unit, unit_price,
		                            vat_code, vat_rate_bp, net, vat, gross)
		   VALUES (10, 1, 'Tanacsadas', 1000000, 'ora', 1000, 'STD27', 2700, 1000, 270, 1270);
		 INSERT INTO invoice_vat_groups (invoice_id, vat_code, vat_rate_bp, net, vat, gross)
		   VALUES (10, 'STD27', 2700, 1000, 270, 1270);
		 INSERT INTO invoice_documents (invoice_id, sha256, bytes, template_version, rendered_at)
		   VALUES (10, 'deadbeef', 4096, 'v1', 9);
		 INSERT INTO nav_submissions (id, invoice_id, op, verdict, created_at, done_at)
		   VALUES (5, 10, 'CREATE', 'DONE', 9, 9);
		 INSERT INTO payments
		   (id, uid, tenant_id, kind, status, amount, currency, created_at, updated_at)
		   VALUES (42, 'pay_a', 1, 'TRANSFER', 'SUCCEEDED', 1270, 'HUF', 7, 7);
		 INSERT INTO payment_allocations (payment_id, invoice_id, amount, allocated_at)
		   VALUES (42, 10, 1270, 9);",
	)
	.execute(&mut *store.writer().acquire().await.unwrap())
	.await
	.unwrap();

	// The constraint a shipped database carries, and the whole reason the table is rebuilt.
	sqlx::query("UPDATE invoices SET status = 'PENDING' WHERE id = 10")
		.execute(store.writer())
		.await
		.expect_err("version 6 has no PENDING");

	store.migrate(&[FRAMEWORK]).await.unwrap();

	// Ids verbatim, which is what keeps everything pointing at them.
	for (table, expected) in [
		("invoices", 2_i64),
		("invoice_lines", 1),
		("invoice_vat_groups", 1),
		("invoice_documents", 1),
		("nav_submissions", 1),
		("payment_allocations", 1),
	] {
		let n: i64 =
			sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
				.fetch_one(store.reader())
				.await
				.unwrap();
		assert_eq!(n, expected, "{table} lost rows");
	}
	let row = sqlx::query("SELECT * FROM invoices WHERE id = 10")
		.fetch_one(store.reader())
		.await
		.unwrap();
	assert_eq!(row.get::<String, _>("uid"), "inv_orig");
	assert_eq!(row.get::<String, _>("number"), "A2026/000001");
	assert_eq!(row.get::<i64, _>("gross"), 1270);
	let orphans: i64 = sqlx::query_scalar(
		"SELECT count(*) FROM payment_allocations a
		  LEFT JOIN invoices i ON i.id = a.invoice_id WHERE i.id IS NULL",
	)
	.fetch_one(store.reader())
	.await
	.unwrap();
	assert_eq!(orphans, 0);

	// The widened CHECK, and the guard the rebuild had to recreate by hand.
	sqlx::query("UPDATE invoices SET status = 'PENDING' WHERE id = 10")
		.execute(store.writer())
		.await
		.unwrap();
	sqlx::query(
		"INSERT INTO invoices (uid, tenant_id, seller_id, seller_ver, kind, status, number,
		                       issued_at, fulfilment_date, buyer_name, currency,
		                       original_invoice_id, created_at, updated_at)
		   VALUES ('inv_second_storno', 1, 1, 1, 'STORNO', 'ISSUED', 'A2026/000003',
		           9, '2026-01-01', 'Vevo Bt.', 'HUF', 10, 9, 9)",
	)
	.execute(store.writer())
	.await
	.expect_err("idx_invoice_storno_once must refuse a second storno");

	// And the shape is where a fresh install lands.
	let fresh_db = TmpDb::new("invoices-v6-fresh");
	let fresh = open(&fresh_db).await;
	fresh.migrate(&[FRAMEWORK]).await.unwrap();
	let (a, b) = (shape(store.reader()).await, shape(fresh.reader()).await);
	assert_eq!(a["invoices"], b["invoices"], "invoices differs from a fresh install");
}

/// The shape parity test above covers the columns; this one covers the rows. Version 3 moves
/// `request_xml`/`response_xml` into `nav_submission_xml`.
#[tokio::test]
async fn the_nav_archive_xml_moves_to_its_own_table() {
	let db = TmpDb::new("nav-archive-moved");
	let store = open_v1(&db).await;

	// `nav_submissions.invoice_id` is a real FK and the runner runs `PRAGMA foreign_key_check`
	// before committing, so the whole parent chain has to exist.
	sqlx::raw_sql(
		"INSERT INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_1', 'a@e.st', 0);
		 INSERT INTO tenants (id, uid, kind, name, owner_account_id, created_at)
			VALUES (1, 'tnt_1', 'O', 'T', 1, 0);
		 INSERT INTO sellers (id, name, tax_number, postcode, city, street, nav_base_url,
				created_at)
			VALUES (1, 'S', '12345678242', '1011', 'Bp', 'Fo 1', 'https://x', 0);
		 INSERT INTO invoices (id, uid, tenant_id, seller_id, currency, created_at, updated_at)
			VALUES (1, 'inv_1', 1, 1, 'HUF', 0, 0), (2, 'inv_2', 1, 1, 'HUF', 0, 0);
		 INSERT INTO nav_submissions (id, invoice_id, op, request_xml, response_xml, created_at)
			VALUES (1, 1, 'CREATE', '<req/>', '<rep/>', 0), (2, 2, 'CREATE', NULL, NULL, 0);",
	)
	.execute(store.writer())
	.await
	.unwrap();

	store.migrate(&[FRAMEWORK]).await.unwrap();

	let archived: Vec<(i64, String, String)> =
		sqlx::query_as("SELECT submission_id, request_xml, response_xml FROM nav_submission_xml")
			.fetch_all(store.reader())
			.await
			.unwrap();
	assert_eq!(archived, vec![(1, "<req/>".into(), "<rep/>".into())], "only the row with XML");
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
			.map_err(|e| saas_core::error::Error::internal(e.to_string()))?;
		Ok(())
	})
}

fn v2(conn: &mut sqlx::SqliteConnection, from: i64) -> Fut<'_> {
	Box::pin(async move {
		SEEN.store(from, Ordering::SeqCst);
		sqlx::raw_sql("ALTER TABLE toy ADD COLUMN label TEXT")
			.execute(conn)
			.await
			.map_err(|e| saas_core::error::Error::internal(e.to_string()))?;
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
			.fetch_one(store.reader())
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
			.map_err(|e| saas_core::error::Error::internal(e.to_string()))?;
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
			.map_err(|e| saas_core::error::Error::internal(e.to_string()))?;
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
	assert!(!has_table(store.reader(), "dupe").await, "neither pass may have run its DDL");
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
		.execute(store.writer())
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
		.map_err(|e| saas_core::error::Error::internal(e.to_string()))?;
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
	assert!(!has_table(store.reader(), "child").await, "the transaction must have rolled back");
	// `schema_version` is created inside the same transaction, so its absence is the proof that
	// nothing was stamped.
	assert!(!has_table(store.reader(), "schema_version").await);
}

fn first(conn: &mut sqlx::SqliteConnection, _from: i64) -> Fut<'_> {
	Box::pin(async move {
		sqlx::raw_sql("CREATE TABLE ordered_first (id INTEGER PRIMARY KEY)")
			.execute(conn)
			.await
			.map_err(|e| saas_core::error::Error::internal(e.to_string()))?;
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
		.map_err(|e| saas_core::error::Error::internal(e.to_string()))?;
		Ok(())
	})
}

fn boom(_conn: &mut sqlx::SqliteConnection, _from: i64) -> Fut<'_> {
	Box::pin(async move { Err(saas_core::error::Error::internal("boom")) })
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
	assert!(has_table(store.reader(), "ordered_first").await);

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
		!has_table(rollback.reader(), "ordered_first").await,
		"a later module's failure must roll the earlier one back too"
	);
}

// vim: ts=4
