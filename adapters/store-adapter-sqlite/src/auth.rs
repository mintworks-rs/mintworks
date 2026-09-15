//! `AuthStore` over SQLite. Reads go through `reader()`, writes through `writer()`.
//!
//! `saas-auth` carries no driver dependency, so no row type here can be decoded by derive:
//! every query binds primitives and every framework row is built by hand in the `*_row`
//! helpers below. See `util.rs` for the conversion vocabulary.

use async_trait::async_trait;
// The *same* function `gdpr::rescale` looks these keys back up with, not a second copy.
use saas_auth::gdpr::camel;
use saas_auth::store::{
	Account, AccountStatus, AccountTenant, ApiKey, AuthStore, Consent, ErasedCol, ErasurePlan,
	ExportScope, ExportSection, LegalDoc, LegalKind, Member, NewAccount, NewApiKey, NewConsent,
	NewLegalDoc, NewTotpCredential, Role, Tenant, TenantKind, TenantStatus, TotpCredential,
};
use saas_core::error::StatusCode;
use saas_core::prelude::*;
use serde_json::Value;
use sqlx::{Row, SqliteConnection, sqlite::SqliteRow};

use crate::util::unique_as_conflict;
use crate::{
	SqliteStore,
	util::{DbExt, RowExt, RowsExt},
};

// ---------------------------------------------------------------- row mapping
//
// Every read below is **by column name**: `SELECT *` and `RETURNING *` follow the DDL's column
// order, which a migration may change, and three queries alias or add columns. Reading by index
// would silently misalign.

fn account_row(row: &SqliteRow) -> ClResult<Account> {
	Ok(Account {
		id: row.try_get("id").db()?,
		uid: AccountId::from_trusted(row.try_get::<String, _>("uid").db()?),
		email: row.try_get("email").db()?,
		pwd_hash: row.try_get("pwd_hash").db()?,
		name: row.try_get("name").db()?,
		locale: row.try_get("locale").db()?,
		status: row.try_get::<String, _>("status").db()?.parse()?,
		token_epoch: row.try_get("token_epoch").db()?,
		is_operator: row.try_get("is_operator").db()?,
		failed_logins: row.try_get("failed_logins").db()?,
		locked_until: row.try_get::<Option<i64>, _>("locked_until").db()?.map(Timestamp),
		activated_at: row.try_get::<Option<i64>, _>("activated_at").db()?.map(Timestamp),
		last_login_at: row.try_get::<Option<i64>, _>("last_login_at").db()?.map(Timestamp),
		anonymized_at: row.try_get::<Option<i64>, _>("anonymized_at").db()?.map(Timestamp),
		created_at: Timestamp(row.try_get("created_at").db()?),
	})
}

fn tenant_row(row: &SqliteRow) -> ClResult<Tenant> {
	Ok(Tenant {
		id: row.try_get("id").db()?,
		uid: TenantId::from_trusted(row.try_get::<String, _>("uid").db()?),
		kind: row.try_get::<String, _>("kind").db()?.parse()?,
		name: row.try_get("name").db()?,
		owner_account_id: row.try_get("owner_account_id").db()?,
		billing_currency: row
			.try_get::<Option<String>, _>("billing_currency")
			.db()?
			.map(CurrencyCode::from_trusted),
		status: row.try_get::<String, _>("status").db()?.parse()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
	})
}

fn account_tenant_row(row: &SqliteRow) -> ClResult<AccountTenant> {
	Ok(AccountTenant {
		uid: TenantId::from_trusted(row.try_get::<String, _>("uid").db()?),
		kind: row.try_get::<String, _>("kind").db()?.parse()?,
		name: row.try_get("name").db()?,
		status: row.try_get::<String, _>("status").db()?.parse()?,
		role: row.try_get::<String, _>("role").db()?.parse()?,
		accepted_at: row.try_get::<Option<i64>, _>("accepted_at").db()?.map(Timestamp),
	})
}

fn member_row(row: &SqliteRow) -> ClResult<Member> {
	Ok(Member {
		account_uid: AccountId::from_trusted(row.try_get::<String, _>("account_uid").db()?),
		email: row.try_get("email").db()?,
		name: row.try_get("name").db()?,
		role: row.try_get::<String, _>("role").db()?.parse()?,
		// NULL for a pending membership — the `CASE` in `members` masks it.
		status: row
			.try_get::<Option<String>, _>("status")
			.db()?
			.map(|s| s.parse())
			.transpose()?,
		accepted: row.try_get("accepted").db()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
	})
}

fn api_key_row(row: &SqliteRow) -> ClResult<ApiKey> {
	Ok(ApiKey {
		id: row.try_get("id").db()?,
		uid: ApiKeyId::from_trusted(row.try_get::<String, _>("uid").db()?),
		tenant_id: row.try_get("tenant_id").db()?,
		account_id: row.try_get("account_id").db()?,
		name: row.try_get("name").db()?,
		prefix: row.try_get("prefix").db()?,
		key_hash: row.try_get("key_hash").db()?,
		scopes: row.try_get("scopes").db()?,
		last_used_at: row.try_get::<Option<i64>, _>("last_used_at").db()?.map(Timestamp),
		expires_at: row.try_get::<Option<i64>, _>("expires_at").db()?.map(Timestamp),
		revoked_at: row.try_get::<Option<i64>, _>("revoked_at").db()?.map(Timestamp),
		created_at: Timestamp(row.try_get("created_at").db()?),
	})
}

fn totp_row(row: &SqliteRow) -> ClResult<TotpCredential> {
	Ok(TotpCredential {
		account_id: row.try_get("account_id").db()?,
		secret_nonce: row.try_get("secret_nonce").db()?,
		secret_enc: row.try_get("secret_enc").db()?,
		digits: row.try_get("digits").db()?,
		period: row.try_get("period").db()?,
		recovery_hashes: row.try_get("recovery_hashes").db()?,
		last_used_step: row.try_get("last_used_step").db()?,
		confirmed_at: row.try_get::<Option<i64>, _>("confirmed_at").db()?.map(Timestamp),
		created_at: Timestamp(row.try_get("created_at").db()?),
	})
}

fn legal_doc_row(row: &SqliteRow) -> ClResult<LegalDoc> {
	Ok(LegalDoc {
		id: row.try_get("id").db()?,
		kind: row.try_get::<String, _>("kind").db()?.parse()?,
		locale: row.try_get("locale").db()?,
		version: row.try_get("version").db()?,
		title: row.try_get("title").db()?,
		body: row.try_get("body").db()?,
		sha256: row.try_get("sha256").db()?,
		effective_from: Timestamp(row.try_get("effective_from").db()?),
	})
}

fn consent_row(row: &SqliteRow) -> ClResult<Consent> {
	Ok(Consent {
		id: row.try_get("id").db()?,
		kind: row.try_get::<String, _>("kind").db()?.parse()?,
		tenant_uid: row
			.try_get::<Option<String>, _>("tenant_uid")
			.db()?
			.map(TenantId::from_trusted),
		doc_version: row.try_get("doc_version").db()?,
		doc_sha256: row.try_get("doc_sha256").db()?,
		granted: row.try_get("granted").db()?,
		at: Timestamp(row.try_get("at").db()?),
		withdrawn_at: row.try_get::<Option<i64>, _>("withdrawn_at").db()?.map(Timestamp),
	})
}

/// [`dump`]'s alias for the row being exported. Not a table name in this schema, so a
/// correlated subquery over the *same* table cannot shadow it.
const SRC: &str = "\"_src\"";

/// Every row of `table` matching `where_sql` as a JSON array, restricted to `columns` — the
/// closed allowlist `saas-auth` hands down (`saas_auth::gdpr::EXPORT`). **Which** columns
/// leave the database is that crate's decision; this function only decides how each one is
/// rendered.
///
/// This is a subject access request, not a database dump, so the shape is the wire's and the
/// internals stay in:
///
/// * An INTEGER `*_id` becomes the referenced row's `uid` under an `…Uid` name, and is dropped
///   where the column declares no foreign key or the referenced table has no `uid`
///   (`legal_docs`). A **TEXT** `*_id` is already public — `audit_logs.entity_id` is a
///   prefixed ULID — and is kept.
/// * Keys are camelCase, matching every other JSON this framework emits.
/// * Timestamps are ISO-8601 UTC, recognised by the `*_at` naming plus `audit_logs.at`.
///
/// A column the allowlist names and the table does not have is an error, not a silent
/// omission: `saas-auth` carries column names for tables it does not own, so drift has to be
/// loud. An absent *table* is the caller's case, not this one.
///
/// A column in `mask` renders NULL on every row its predicate excludes, keeping the key.
///
/// `where_sql` takes exactly one `?`, bound to `id`. Table and column names, and the mask
/// predicates, come from a `&'static` allowlist and from the schema, never from a request, so
/// the interpolation below cannot carry user input.
///
/// The outer table is aliased [`SRC`] so a correlated subquery can name its columns: see the
/// self-referential-FK case in the loop.
async fn dump(
	conn: &mut SqliteConnection,
	table: &str,
	where_sql: &str,
	id: i64,
	columns: &[&str],
	mask: &[(&str, &str)],
) -> ClResult<Value> {
	let types: Vec<(String, String)> =
		sqlx::query_as("SELECT name, type FROM pragma_table_info(?)")
			.bind(table)
			.fetch_all(&mut *conn)
			.await
			.db()?;
	let fks: Vec<(String, String)> =
		sqlx::query_as("SELECT \"from\", \"table\" FROM pragma_foreign_key_list(?)")
			.bind(table)
			.fetch_all(&mut *conn)
			.await
			.db()?;

	let mut pairs: Vec<String> = Vec::with_capacity(columns.len());
	for &col in columns {
		let Some((_, ty)) = types.iter().find(|(name, _)| name.as_str() == col) else {
			return Err(Error::internal(format!("export: {table} has no column {col}")));
		};
		let mut key = camel(col);
		let value = if let Some(stem) = col.strip_suffix("_id")
			&& ty.eq_ignore_ascii_case("INTEGER")
		{
			let Some((_, referenced)) = fks.iter().find(|(from, _)| from == col) else {
				continue;
			};
			if !has_column(&mut *conn, referenced, "uid").await? {
				continue;
			}
			key = camel(&format!("{stem}_uid"));
			// Qualified with the outer query's *alias*, because `invoices.original_invoice_id`
			// is self-referential: unqualified — or qualified by table name, which the subquery's
			// FROM re-binds — it degenerates to "an invoice that cancels itself", always NULL.
			format!("(SELECT \"uid\" FROM \"{referenced}\" WHERE id = {SRC}.\"{col}\")")
		} else if col == "at" || col.ends_with("_at") {
			format!("strftime('%Y-%m-%dT%H:%M:%SZ', \"{col}\", 'unixepoch')")
		} else {
			format!("\"{col}\"")
		};
		// A masked column keeps its key and loses its value on the rows the predicate excludes.
		// Dropping `audit_logs.entity_id` outright instead left every action that passes
		// `detail: None` — `TENANT_DELETED`, all of `saas-invoice`'s — an unidentifiable stub.
		let value = match mask.iter().find(|(name, _)| *name == col) {
			Some((_, predicate)) => format!("CASE WHEN {predicate} THEN {value} END"),
			None => value,
		};
		pairs.push(format!("'{key}', {value}"));
	}
	if pairs.is_empty() {
		return Ok(Value::Array(Vec::new()));
	}
	let sql = format!(
		"SELECT COALESCE(json_group_array(json_object({})), '[]') \
		 FROM \"{table}\" AS {SRC} WHERE {where_sql}",
		pairs.join(", ")
	);
	let raw: String = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
		.bind(id)
		.fetch_one(conn)
		.await
		.db()?;
	serde_json::from_str(&raw).map_err(|e| Error::internal(format!("export {table}: {e}")))
}

/// Whether `table` has a column called `column` — how [`dump`] decides that a foreign key
/// can be resolved to a public `uid` rather than dropped.
async fn has_column(conn: &mut SqliteConnection, table: &str, column: &str) -> ClResult<bool> {
	let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pragma_table_info(?) WHERE name = ?")
		.bind(table)
		.bind(column)
		.fetch_one(conn)
		.await
		.db()?;
	Ok(n > 0)
}

/// The `WHERE` fragment behind each [`ExportScope`], taking exactly one `?` bound to the
/// account id. The scopes themselves — which table is reached which way — are
/// `saas_auth::gdpr::EXPORT`'s; this is only their SQL.
fn where_of(scope: ExportScope) -> &'static str {
	match scope {
		ExportScope::Account => "id = ?",
		ExportScope::AccountId => "account_id = ?",
		ExportScope::MemberTenant => {
			"id IN (SELECT tenant_id FROM memberships WHERE account_id = ?)"
		}
		ExportScope::PersonalTenant => {
			"tenant_id IN (SELECT id FROM tenants WHERE owner_account_id = ? AND kind = 'P')"
		}
		// Lines and VAT groups ship as sibling arrays rather than nested inside
		// each invoice — the same evidence, no join logic. Nest them if a reader needs it.
		ExportScope::PersonalTenantInvoice => {
			"invoice_id IN (SELECT id FROM invoices WHERE \
			 tenant_id IN (SELECT id FROM tenants WHERE owner_account_id = ? AND kind = 'P'))"
		}
	}
}

/// `"a" = ?, "b" = ?` for an erasure allowlist, empty when the allowlist is. Column names
/// come from `saas_auth::gdpr::ERASURE`, a `&'static` value in the feature crate and never
/// from a request; the replacement literals are **bound**, in the same order.
fn set_clause(cols: &[ErasedCol]) -> String {
	cols.iter()
		.map(|(col, _)| format!("\"{col}\" = ?"))
		.collect::<Vec<_>>()
		.join(", ")
}

/// `saas-invoice`'s tables are absent in a deployment that does not use it.
///
/// Generic over the executor so a caller inside a write transaction can ask *on that
/// transaction*. Asking the reader pool mid-transaction is a read-modify-write across two
/// connections — harmless while the answer is only schema presence, a bug the moment it
/// touches rows.
async fn has_table<'e, E>(ex: E, name: &str) -> ClResult<bool>
where
	E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
	let n: i64 =
		sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?")
			.bind(name)
			.fetch_one(ex)
			.await
			.db()?;
	Ok(n > 0)
}

#[async_trait]
impl AuthStore for SqliteStore {
	// -- accounts

	async fn create_account(
		&self,
		new: &NewAccount,
		consents: &[NewConsent],
		join: Option<(i64, Role)>,
	) -> ClResult<(Account, Tenant)> {
		let now = Timestamp::now();
		let mut tx = self.write_tx().await?;

		// Bound to a `let` so the borrow in `.bind(uid.as_str())` outlives the statement.
		let account_uid = AccountId::generate();
		let row = sqlx::query(
			"INSERT INTO accounts (uid, email, pwd_hash, name, locale, created_at)
			 VALUES (?, ?, ?, ?, ?, ?) RETURNING *",
		)
		.bind(account_uid.as_str())
		.bind(&new.email)
		.bind(&new.pwd_hash)
		.bind(&new.name)
		.bind(&new.locale)
		.bind(now.0)
		.fetch_one(&mut *tx)
		.await
		.map_err(|e| unique_as_conflict(&e, "email already registered"))?;
		let account = account_row(&row)?;

		let tenant_uid = TenantId::generate();
		let row = sqlx::query(
			"INSERT INTO tenants (uid, kind, name, owner_account_id, created_at)
			 VALUES (?, 'P', ?, ?, ?) RETURNING *",
		)
		.bind(tenant_uid.as_str())
		.bind(&new.tenant_name)
		.bind(account.id)
		.bind(now.0)
		.fetch_one(&mut *tx)
		.await
		.db()?;
		let tenant = tenant_row(&row)?;

		// The owner does not invite themselves, so this membership is accepted on creation.
		// Without `accepted_at`, `saas_auth::token::pick_tenant` skips it and login mints a
		// token with no `tnt` claim.
		sqlx::query(
			"INSERT INTO memberships (tenant_id, account_id, role, accepted_at, created_at)
			 VALUES (?, ?, 'OWNER', ?, ?)",
		)
		.bind(tenant.id)
		.bind(account.id)
		.bind(now.0)
		.bind(now.0)
		.execute(&mut *tx)
		.await
		.db()?;

		// The invite's membership, in the same transaction for the same reason the consents
		// are. No `accepted_at`: the invitee accepts by entering the tenant through
		// `switch-tenant`, which is what `auth_mw`'s tenant join requires.
		if let Some((tenant_id, role)) = join {
			sqlx::query(
				"INSERT INTO memberships (tenant_id, account_id, role, created_at)
				 VALUES (?, ?, ?, ?)",
			)
			.bind(tenant_id)
			.bind(account.id)
			.bind(role.as_str())
			.bind(now.0)
			.execute(&mut *tx)
			.await
			.db()?;
		}

		// In the same transaction as the account: written outside it, a `SQLITE_BUSY` left an
		// account with no ToS/privacy rows, which `consent::gate` then blocks on every gated
		// route — unusable and unrepairable. `account_id` comes from the row just inserted.
		for c in consents {
			sqlx::query(
				"INSERT INTO consents
					(account_id, tenant_id, kind, legal_doc_id, doc_version, doc_sha256,
					 granted, at, ip, user_agent)
				 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
			)
			.bind(account.id)
			.bind(c.tenant_id)
			.bind(c.kind.as_str())
			.bind(c.legal_doc_id)
			.bind(&c.doc_version)
			.bind(&c.doc_sha256)
			.bind(c.granted)
			.bind(now.0)
			.bind(&c.ip)
			.bind(&c.user_agent)
			.execute(&mut *tx)
			.await
			.db()?;
		}

		tx.commit().await.db()?;
		Ok((account, tenant))
	}

	async fn account_by_email(&self, email: &str) -> ClResult<Option<Account>> {
		sqlx::query("SELECT * FROM accounts WHERE email = ?")
			.bind(email)
			.fetch_optional(self.reader())
			.await
			.one(account_row)
	}

	async fn account_by_uid(&self, uid: &AccountId) -> ClResult<Option<Account>> {
		sqlx::query("SELECT * FROM accounts WHERE uid = ?")
			.bind(uid.as_str())
			.fetch_optional(self.reader())
			.await
			.one(account_row)
	}

	async fn account_by_id(&self, id: i64) -> ClResult<Option<Account>> {
		sqlx::query("SELECT * FROM accounts WHERE id = ?")
			.bind(id)
			.fetch_optional(self.reader())
			.await
			.one(account_row)
	}

	async fn activate_account(
		&self,
		id: i64,
		pwd_hash: Option<&str>,
		at: Timestamp,
	) -> ClResult<bool> {
		let res = sqlx::query(
			"UPDATE accounts SET status = 'ACTIVE', activated_at = ?,
				 pwd_hash = COALESCE(?, pwd_hash)
			 WHERE id = ? AND status = 'PENDING'",
		)
		.bind(at.0)
		.bind(pwd_hash)
		.bind(id)
		.execute(self.writer())
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn set_password(&self, id: i64, expected_epoch: i64, pwd_hash: &str) -> ClResult<bool> {
		let res = sqlx::query(
			"UPDATE accounts SET pwd_hash = ?, token_epoch = token_epoch + 1
			 WHERE id = ? AND token_epoch = ?",
		)
		.bind(pwd_hash)
		.bind(id)
		.bind(expected_epoch)
		.execute(self.writer())
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn bump_token_epoch(&self, id: i64) -> ClResult<()> {
		sqlx::query("UPDATE accounts SET token_epoch = token_epoch + 1 WHERE id = ?")
			.bind(id)
			.execute(self.writer())
			.await
			.db()?;
		Ok(())
	}

	/// The `WHERE` predicate is the whole guarantee that GDPR erasure is irreversible: without
	/// it an operator un-suspending a batch — or a consumer calling the trait directly — moves
	/// an anonymized account (`email = 'anonymized+…@invalid'`, `pwd_hash = NULL`) back to
	/// `ACTIVE`, which `auth_mw::account_for_token` then accepts. Hardcoded in the SQL like
	/// `InvoiceStore::mark_paid`/`mark_stornoed`; a consumer's own raw SQL is its own
	/// responsibility (`adapter-contract.md` §5).
	async fn set_account_status(&self, id: i64, status: AccountStatus) -> ClResult<()> {
		// `write_tx` for the same reason as `anonymize_account`: the epoch bump is the whole
		// point of a suspension, and as a second connection's write it could fail alone and
		// leave the account SUSPENDED with every issued token still live.
		let mut tx = self.write_tx().await?;
		let res = sqlx::query(
			"UPDATE accounts SET status = ?
			  WHERE id = ? AND (status <> 'ANONYMIZED' OR ? = 'ANONYMIZED')",
		)
		.bind(status.as_str())
		.bind(id)
		.bind(status.as_str())
		.execute(&mut *tx)
		.await
		.db()?;
		if res.rows_affected() == 0 {
			// Zero rows is either no such account — which the trait's `ClResult<()>` has
			// always reported as nothing to do — or the one case worth naming. One extra read,
			// on a path that is already an error.
			let current: Option<String> =
				sqlx::query_scalar("SELECT status FROM accounts WHERE id = ?")
					.bind(id)
					.fetch_optional(&mut *tx)
					.await
					.db()?;
			if current.as_deref() == Some("ANONYMIZED") {
				return Err(Error::coded(
					StatusCode::CONFLICT,
					"E-AUTH-ANONYMIZED",
					"an erased account cannot be reactivated",
				));
			}
		} else if status == AccountStatus::Suspended {
			sqlx::query("UPDATE accounts SET token_epoch = token_epoch + 1 WHERE id = ?")
				.bind(id)
				.execute(&mut *tx)
				.await
				.db()?;
		}
		tx.commit().await.db()?;
		Ok(())
	}

	async fn record_login_failure(&self, id: i64) -> ClResult<()> {
		// Unconditional, and it must stay that way: `id` is `login::NO_ACCOUNT` on the
		// unknown-address branch, where matching no row is the point and paying the writer
		// round trip anyway is what keeps the branch indistinguishable.
		sqlx::query("UPDATE accounts SET failed_logins = failed_logins + 1 WHERE id = ?")
			.bind(id)
			.execute(self.writer())
			.await
			.db()?;
		Ok(())
	}

	async fn record_login_success(&self, id: i64, at: Timestamp) -> ClResult<()> {
		sqlx::query(
			"UPDATE accounts SET failed_logins = 0, locked_until = NULL, last_login_at = ?
			 WHERE id = ?",
		)
		.bind(at.0)
		.bind(id)
		.execute(self.writer())
		.await
		.db()?;
		Ok(())
	}

	// -- tenants

	async fn create_tenant(
		&self,
		kind: TenantKind,
		name: &str,
		owner_account_id: i64,
		billing_currency: Option<&CurrencyCode>,
	) -> ClResult<Tenant> {
		let now = Timestamp::now();
		// `write_tx` for the same reason as `anonymize_account`: a deferred BEGIN cannot
		// upgrade its lock and fails `SQLITE_BUSY` outright under a second writer.
		let mut tx = self.write_tx().await?;

		let tenant_uid = TenantId::generate();
		let row = sqlx::query(
			"INSERT INTO tenants (uid, kind, name, owner_account_id, billing_currency, created_at)
			 VALUES (?, ?, ?, ?, ?, ?) RETURNING *",
		)
		.bind(tenant_uid.as_str())
		.bind(kind.as_str())
		.bind(name)
		.bind(owner_account_id)
		.bind(billing_currency.map(CurrencyCode::as_str))
		.bind(now.0)
		.fetch_one(&mut *tx)
		.await
		.map_err(|e| unique_as_conflict(&e, "this account already has a tenant of that kind"))?;
		let tenant = tenant_row(&row)?;

		// The owner does not invite themselves, so this membership is accepted on creation.
		sqlx::query(
			"INSERT INTO memberships (tenant_id, account_id, role, accepted_at, created_at)
			 VALUES (?, ?, 'OWNER', ?, ?)",
		)
		.bind(tenant.id)
		.bind(owner_account_id)
		.bind(now.0)
		.bind(now.0)
		.execute(&mut *tx)
		.await
		.db()?;

		tx.commit().await.db()?;
		Ok(tenant)
	}

	async fn tenant_by_uid(&self, uid: &TenantId) -> ClResult<Option<Tenant>> {
		sqlx::query("SELECT * FROM tenants WHERE uid = ?")
			.bind(uid.as_str())
			.fetch_optional(self.reader())
			.await
			.one(tenant_row)
	}

	async fn tenant_by_id(&self, id: i64) -> ClResult<Option<Tenant>> {
		sqlx::query("SELECT * FROM tenants WHERE id = ?")
			.bind(id)
			.fetch_optional(self.reader())
			.await
			.one(tenant_row)
	}

	async fn update_tenant(
		&self,
		id: i64,
		name: Option<&str>,
		billing_currency: Patch<CurrencyCode>,
		status: Option<TenantStatus>,
	) -> ClResult<()> {
		let currency = billing_currency.as_option();
		sqlx::query(
			"UPDATE tenants SET
				name = COALESCE(?, name),
				billing_currency = CASE WHEN ? THEN ? ELSE billing_currency END,
				status = COALESCE(?, status)
			 WHERE id = ?",
		)
		.bind(name)
		.bind(currency.is_some())
		.bind(currency.flatten().map(CurrencyCode::as_str))
		.bind(status.map(TenantStatus::as_str))
		.bind(id)
		.execute(self.writer())
		.await
		.db()?;
		Ok(())
	}

	async fn transfer_tenant_ownership(
		&self,
		tenant_id: i64,
		from: i64,
		to: i64,
	) -> ClResult<bool> {
		let mut tx = self.write_tx().await?;

		// Both predicates re-run inside the transaction, as `anonymize_account` re-runs its
		// own: the service checked them on the reader pool, where a concurrent `remove_member`
		// is invisible.
		let ok: Option<i64> = sqlx::query_scalar(
			"SELECT 1 FROM tenants t \
			  JOIN memberships m ON m.tenant_id = t.id AND m.account_id = ? \
			 WHERE t.id = ? AND t.owner_account_id = ? AND m.accepted_at IS NOT NULL",
		)
		.bind(to)
		.bind(tenant_id)
		.bind(from)
		.fetch_optional(&mut *tx)
		.await
		.db()?;
		if ok.is_none() {
			return Ok(false);
		}

		// Demoted to `ADMIN`, not removed: an owner handing the organisation over is still a
		// member of it, and `remove_member` is the separate decision.
		for (account_id, role) in [(from, "ADMIN"), (to, "OWNER")] {
			sqlx::query("UPDATE memberships SET role = ? WHERE tenant_id = ? AND account_id = ?")
				.bind(role)
				.bind(tenant_id)
				.bind(account_id)
				.execute(&mut *tx)
				.await
				.db()?;
		}
		sqlx::query("UPDATE tenants SET owner_account_id = ? WHERE id = ?")
			.bind(to)
			.bind(tenant_id)
			.execute(&mut *tx)
			.await
			.db()?;

		tx.commit().await.db()?;
		Ok(true)
	}

	async fn delete_tenant(&self, tenant_id: i64) -> ClResult<bool> {
		let mut tx = self.write_tx().await?;

		let others: i64 = sqlx::query_scalar(
			"SELECT count(*) FROM memberships m JOIN tenants t ON t.id = m.tenant_id \
			  WHERE m.tenant_id = ? AND m.accepted_at IS NOT NULL \
				AND m.account_id != t.owner_account_id",
		)
		.bind(tenant_id)
		.fetch_one(&mut *tx)
		.await
		.db()?;
		if others > 0 {
			return Ok(false);
		}

		// `invoices` is the eight-year retention obligation and `consents` is evidence; neither
		// FK cascades, so without this the delete failed as an opaque constraint error. A
		// deployment without `saas-invoice` has no `invoices` table, hence `has_table`.
		if has_table(&mut *tx, "invoices").await? {
			let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM invoices WHERE tenant_id = ?")
				.bind(tenant_id)
				.fetch_one(&mut *tx)
				.await
				.db()?;
			if kept > 0 {
				return Ok(false);
			}
		}
		let consented: i64 =
			sqlx::query_scalar("SELECT count(*) FROM consents WHERE tenant_id = ?")
				.bind(tenant_id)
				.fetch_one(&mut *tx)
				.await
				.db()?;
		if consented > 0 {
			return Ok(false);
		}

		let gone = sqlx::query("DELETE FROM tenants WHERE id = ? AND kind != 'P'")
			.bind(tenant_id)
			.execute(&mut *tx)
			.await
			.db()?;
		tx.commit().await.db()?;
		Ok(gone.rows_affected() > 0)
	}

	async fn tenants_for_account(&self, account_id: i64) -> ClResult<Vec<AccountTenant>> {
		sqlx::query(
			"SELECT t.uid AS uid, t.kind AS kind, t.name AS name, t.status AS status,
					m.role AS role, m.accepted_at AS accepted_at
			 FROM memberships m JOIN tenants t ON t.id = m.tenant_id
			 WHERE m.account_id = ? ORDER BY t.created_at",
		)
		.bind(account_id)
		.fetch_all(self.reader())
		.await
		.all(account_tenant_row)
	}

	async fn owned_org_tenants(&self, account_id: i64) -> ClResult<Vec<Tenant>> {
		sqlx::query("SELECT * FROM tenants WHERE owner_account_id = ? AND kind != 'P' ORDER BY id")
			.bind(account_id)
			.fetch_all(self.reader())
			.await
			.all(tenant_row)
	}

	async fn currency_enabled(&self, code: &CurrencyCode) -> ClResult<bool> {
		if !has_table(self.reader(), "currencies").await? {
			return Ok(true);
		}
		let n: i64 =
			sqlx::query_scalar("SELECT count(*) FROM currencies WHERE code = ? AND enabled = 1")
				.bind(code.as_str())
				.fetch_one(self.reader())
				.await
				.db()?;
		Ok(n > 0)
	}

	// -- memberships

	async fn membership_role(&self, tenant_id: i64, account_id: i64) -> ClResult<Option<Role>> {
		sqlx::query_scalar::<_, String>(
			"SELECT role FROM memberships WHERE tenant_id = ? AND account_id = ?",
		)
		.bind(tenant_id)
		.bind(account_id)
		.fetch_optional(self.reader())
		.await
		.db()?
		.map(|s| s.parse())
		.transpose()
	}

	async fn accepted_membership_role(
		&self,
		tenant_id: i64,
		account_id: i64,
	) -> ClResult<Option<Role>> {
		sqlx::query_scalar::<_, String>(
			"SELECT role FROM memberships
			 WHERE tenant_id = ? AND account_id = ? AND accepted_at IS NOT NULL",
		)
		.bind(tenant_id)
		.bind(account_id)
		.fetch_optional(self.reader())
		.await
		.db()?
		.map(|s| s.parse())
		.transpose()
	}

	async fn membership_created_at(
		&self,
		tenant_id: i64,
		account_id: i64,
	) -> ClResult<Option<Timestamp>> {
		sqlx::query_scalar::<_, i64>(
			"SELECT created_at FROM memberships WHERE tenant_id = ? AND account_id = ?",
		)
		.bind(tenant_id)
		.bind(account_id)
		.fetch_optional(self.reader())
		.await
		.db()
		.map(|o| o.map(Timestamp))
	}

	async fn accept_membership(
		&self,
		tenant_id: i64,
		account_id: i64,
		at: Timestamp,
	) -> ClResult<()> {
		sqlx::query(
			"UPDATE memberships SET accepted_at = ?
			 WHERE tenant_id = ? AND account_id = ? AND accepted_at IS NULL",
		)
		.bind(at.0)
		.bind(tenant_id)
		.bind(account_id)
		.execute(self.writer())
		.await
		.db()?;
		Ok(())
	}

	async fn put_membership(&self, tenant_id: i64, account_id: i64, role: Role) -> ClResult<bool> {
		// `memberships.role <> 'OWNER'` in the statement, not just in the service: the service
		// checks on the reader pool, where a concurrent `transfer_tenant_ownership` is
		// invisible. Guards only the update branch, so tenant creation still inserts an OWNER.
		let res = sqlx::query(
			"INSERT INTO memberships (tenant_id, account_id, role, created_at)
			 VALUES (?, ?, ?, ?)
			 ON CONFLICT (tenant_id, account_id) DO UPDATE SET role = excluded.role
			   WHERE memberships.role <> 'OWNER'",
		)
		.bind(tenant_id)
		.bind(account_id)
		.bind(role.as_str())
		.bind(Timestamp::now().0)
		.execute(self.writer())
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn remove_membership(&self, tenant_id: i64, account_id: i64) -> ClResult<bool> {
		// No `token_epoch` bump: the epoch is account-wide, so one tenant's admin would sign the
		// account out of every other tenant. The deleted row suffices — `auth_mw`'s tenant join
		// needs a live membership. `role <> 'OWNER'` in the statement, not just in the service:
		// the service checks on the reader pool, where a concurrent transfer is invisible.
		let res = sqlx::query(
			"DELETE FROM memberships WHERE tenant_id = ? AND account_id = ? AND role <> 'OWNER'",
		)
		.bind(tenant_id)
		.bind(account_id)
		.execute(self.writer())
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn members(&self, tenant_id: i64, limit: i64) -> ClResult<Vec<Member>> {
		// A pending membership discloses nothing about the address it names: any caller can post
		// any address, so returning the invitee's `email`, `name` or `status` is a registration
		// oracle plus PII disclosure. Masked in SQL so the columns never leave the database.
		// `account_uid` still leaks a ULID timestamp, so `Auth::members` hides it too.
		sqlx::query(
			"SELECT a.uid AS account_uid,
					CASE WHEN m.accepted_at IS NULL THEN NULL ELSE a.email END AS email,
					CASE WHEN m.accepted_at IS NULL THEN NULL ELSE a.name END AS name,
					m.role AS role,
					CASE WHEN m.accepted_at IS NULL THEN NULL ELSE a.status END AS status,
					m.accepted_at IS NOT NULL AS accepted,
					m.created_at AS created_at
			 FROM memberships m JOIN accounts a ON a.id = m.account_id
			 WHERE m.tenant_id = ? ORDER BY m.created_at LIMIT ?",
		)
		.bind(tenant_id)
		.bind(limit)
		.fetch_all(self.reader())
		.await
		.all(member_row)
	}

	// -- api keys

	async fn create_api_key(&self, new: &NewApiKey) -> ClResult<ApiKey> {
		let uid = ApiKeyId::generate();
		let row = sqlx::query(
			"INSERT INTO api_keys
				(uid, tenant_id, account_id, name, prefix, key_hash, scopes, expires_at,
				 created_at)
			 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING *",
		)
		.bind(uid.as_str())
		.bind(new.tenant_id)
		.bind(new.account_id)
		.bind(&new.name)
		.bind(&new.prefix)
		.bind(&new.key_hash)
		.bind(&new.scopes)
		.bind(new.expires_at.map(|t| t.0))
		.bind(Timestamp::now().0)
		.fetch_one(self.writer())
		.await
		.map_err(|e| unique_as_conflict(&e, "api key prefix collision"))?;
		api_key_row(&row)
	}

	async fn api_key_by_prefix(&self, prefix: &str) -> ClResult<Option<ApiKey>> {
		sqlx::query("SELECT * FROM api_keys WHERE prefix = ?")
			.bind(prefix)
			.fetch_optional(self.reader())
			.await
			.one(api_key_row)
	}

	async fn api_keys_for_tenant(&self, tenant_id: i64) -> ClResult<Vec<ApiKey>> {
		sqlx::query("SELECT * FROM api_keys WHERE tenant_id = ? ORDER BY created_at")
			.bind(tenant_id)
			.fetch_all(self.reader())
			.await
			.all(api_key_row)
	}

	async fn touch_api_key(&self, id: i64, at: Timestamp) -> ClResult<()> {
		sqlx::query("UPDATE api_keys SET last_used_at = ? WHERE id = ?")
			.bind(at.0)
			.bind(id)
			.execute(self.writer())
			.await
			.db()?;
		Ok(())
	}

	async fn revoke_api_key(
		&self,
		tenant_id: i64,
		uid: &ApiKeyId,
		at: Timestamp,
	) -> ClResult<bool> {
		let res = sqlx::query(
			"UPDATE api_keys SET revoked_at = ?
			  WHERE uid = ? AND tenant_id = ? AND revoked_at IS NULL",
		)
		.bind(at.0)
		.bind(uid.as_str())
		.bind(tenant_id)
		.execute(self.writer())
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	// -- totp

	/// `confirmed_at IS NULL` is the precondition, carried here rather than read first — see
	/// the trait doc, and every sibling consume-style method in this impl. Reading first lets a
	/// `confirm_totp` land between the read and the upsert and be wiped: 2FA off, and the user
	/// holding printed recovery codes that match nothing.
	async fn put_totp(&self, new: &NewTotpCredential) -> ClResult<bool> {
		let res = sqlx::query(
			"INSERT INTO totp_credentials
				(account_id, secret_nonce, secret_enc, digits, period, recovery_hashes,
				 created_at)
			 VALUES (?, ?, ?, ?, ?, ?, ?)
			 ON CONFLICT (account_id) DO UPDATE SET
				secret_nonce = excluded.secret_nonce,
				secret_enc = excluded.secret_enc,
				digits = excluded.digits,
				period = excluded.period,
				recovery_hashes = excluded.recovery_hashes,
				last_used_step = NULL,
				confirmed_at = NULL
			 WHERE totp_credentials.confirmed_at IS NULL",
		)
		.bind(new.account_id)
		.bind(&new.secret_nonce)
		.bind(&new.secret_enc)
		.bind(new.digits)
		.bind(new.period)
		.bind(&new.recovery_hashes)
		.bind(Timestamp::now().0)
		.execute(self.writer())
		.await
		.db()?;
		Ok(res.rows_affected() > 0)
	}

	async fn totp_by_account(&self, account_id: i64) -> ClResult<Option<TotpCredential>> {
		sqlx::query("SELECT * FROM totp_credentials WHERE account_id = ?")
			.bind(account_id)
			.fetch_optional(self.reader())
			.await
			.one(totp_row)
	}

	async fn confirm_totp(&self, account_id: i64, at: Timestamp, hashes: &str) -> ClResult<bool> {
		// One statement, so no transaction: arming the factor and storing the codes that
		// recover it cannot half-apply. `confirmed_at IS NULL` is the precondition, carried
		// here rather than read first — see the trait doc and every sibling in this impl.
		let res = sqlx::query(
			"UPDATE totp_credentials SET confirmed_at = ?, recovery_hashes = ?
			 WHERE account_id = ? AND confirmed_at IS NULL",
		)
		.bind(at.0)
		.bind(hashes)
		.bind(account_id)
		.execute(self.writer())
		.await
		.db()?;
		Ok(res.rows_affected() > 0)
	}

	async fn advance_totp_step(&self, account_id: i64, step: i64) -> ClResult<bool> {
		let res = sqlx::query(
			"UPDATE totp_credentials SET last_used_step = ?
			 WHERE account_id = ? AND (last_used_step IS NULL OR last_used_step < ?)",
		)
		.bind(step)
		.bind(account_id)
		.bind(step)
		.execute(self.writer())
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn swap_totp_recovery(
		&self,
		account_id: i64,
		expected: &str,
		hashes: &str,
	) -> ClResult<bool> {
		let res = sqlx::query(
			"UPDATE totp_credentials SET recovery_hashes = ?
			 WHERE account_id = ? AND recovery_hashes = ?",
		)
		.bind(hashes)
		.bind(account_id)
		.bind(expected)
		.execute(self.writer())
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn delete_totp(&self, account_id: i64) -> ClResult<bool> {
		let res = sqlx::query("DELETE FROM totp_credentials WHERE account_id = ?")
			.bind(account_id)
			.execute(self.writer())
			.await
			.db()?;
		Ok(res.rows_affected() == 1)
	}

	// -- legal docs and consent

	async fn insert_legal_doc(&self, new: &NewLegalDoc) -> ClResult<i64> {
		sqlx::query_scalar(
			"INSERT INTO legal_docs
				(kind, locale, version, title, body, sha256, effective_from, created_at)
			 VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
		)
		.bind(new.kind.as_str())
		.bind(&new.locale)
		.bind(&new.version)
		.bind(&new.title)
		.bind(&new.body)
		.bind(&new.sha256)
		.bind(new.effective_from.0)
		.bind(Timestamp::now().0)
		.fetch_one(self.writer())
		.await
		.map_err(|e| unique_as_conflict(&e, "this legal document version already exists"))
	}

	async fn current_legal_doc(
		&self,
		kind: LegalKind,
		locale: &str,
		now: Timestamp,
	) -> ClResult<Option<LegalDoc>> {
		let exact: Option<LegalDoc> = sqlx::query(
			"SELECT * FROM legal_docs
			 WHERE kind = ? AND locale = ? AND effective_from <= ?
			 ORDER BY effective_from DESC, id DESC LIMIT 1",
		)
		.bind(kind.as_str())
		.bind(locale)
		.bind(now.0)
		.fetch_optional(self.reader())
		.await
		.one(legal_doc_row)?;
		if exact.is_some() {
			return Ok(exact);
		}
		// Fall back to any published locale, so **a kind published in any locale always gates**:
		// without this an account whose locale has no translation sailed past the consent wall
		// having accepted nothing. Wrong language is the lesser fault.
		sqlx::query(
			"SELECT * FROM legal_docs
			 WHERE kind = ? AND effective_from <= ?
			 ORDER BY effective_from DESC, id DESC LIMIT 1",
		)
		.bind(kind.as_str())
		.bind(now.0)
		.fetch_optional(self.reader())
		.await
		.one(legal_doc_row)
	}

	async fn record_consent(&self, new: &NewConsent, at: Timestamp) -> ClResult<i64> {
		Ok(sqlx::query_scalar(
			"INSERT INTO consents
				(account_id, tenant_id, kind, legal_doc_id, doc_version, doc_sha256, granted,
				 at, ip, user_agent)
			 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
		)
		.bind(new.account_id)
		.bind(new.tenant_id)
		.bind(new.kind.as_str())
		.bind(new.legal_doc_id)
		.bind(&new.doc_version)
		.bind(&new.doc_sha256)
		.bind(new.granted)
		.bind(at.0)
		.bind(&new.ip)
		.bind(&new.user_agent)
		.fetch_one(self.writer())
		.await
		.db()?)
	}

	/// `AND c.tenant_id IS ?`, not `= ?`: in SQLite `IS` compares NULL correctly and behaves
	/// as `=` for everything else, so one bound parameter serves both the account-wide scope
	/// and a tenant-scoped one.
	///
	/// Ordered by `c.id` alone: leading with `c.at` disagreed with `list_consents`' `MAX(id)`
	/// across a clock step back, so the listing showed one row and the withdrawal took another.
	async fn latest_consent(
		&self,
		account_id: i64,
		kind: LegalKind,
		tenant_id: Option<i64>,
	) -> ClResult<Option<Consent>> {
		sqlx::query(
			"SELECT c.*, t.uid AS tenant_uid FROM consents c
			 LEFT JOIN tenants t ON t.id = c.tenant_id
			 WHERE c.account_id = ? AND c.kind = ? AND c.tenant_id IS ?
			 ORDER BY c.id DESC LIMIT 1",
		)
		.bind(account_id)
		.bind(kind.as_str())
		.bind(tenant_id)
		.fetch_optional(self.reader())
		.await
		.one(consent_row)
	}

	/// The newest row per `(kind, tenant_id)`. `MAX(c.id)` picks it — `consents` is
	/// append-only and `id` is monotonic, so the newest id in a group *is* its newest row,
	/// and SQLite's bare-column rule then yields that row's other columns.
	async fn list_consents(&self, account_id: i64) -> ClResult<Vec<Consent>> {
		sqlx::query(
			"SELECT c.*, MAX(c.id) AS newest, t.uid AS tenant_uid FROM consents c
			 LEFT JOIN tenants t ON t.id = c.tenant_id
			 WHERE c.account_id = ?
			 GROUP BY c.kind, c.tenant_id
			 ORDER BY c.kind, c.tenant_id",
		)
		.bind(account_id)
		.fetch_all(self.reader())
		.await
		.all(consent_row)
	}

	async fn withdraw_consent(&self, id: i64, at: Timestamp) -> ClResult<bool> {
		let res = sqlx::query(
			"UPDATE consents SET withdrawn_at = ? WHERE id = ? AND withdrawn_at IS NULL",
		)
		.bind(at.0)
		.bind(id)
		.execute(self.writer())
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	// -- gdpr

	async fn export_account(
		&self,
		account_id: i64,
		sections: &[ExportSection],
	) -> ClResult<Vec<Value>> {
		// One connection, one snapshot: a `dump` per reader-pool connection lets a write land
		// mid-export and tear the document. This is evidence in a GDPR request, so "mostly
		// consistent" is the wrong bar. Deferred, not immediate — a WAL reader blocks nobody.
		let mut tx = self.reader().begin().await.db()?;
		let mut out = Vec::with_capacity(sections.len());
		for section in sections {
			// `[]` rather than a failed export for a section with no columns yet, and for a
			// table this deployment does not have — `saas-invoice`'s, where it is not used.
			let rows = if section.columns.is_empty() || !has_table(&mut *tx, section.table).await? {
				Value::Array(Vec::new())
			} else {
				dump(
					&mut tx,
					section.table,
					where_of(section.scope),
					account_id,
					section.columns,
					section.mask,
				)
				.await?
			};
			out.push(rows);
		}
		tx.rollback().await.db()?;
		Ok(out)
	}

	async fn anonymize_account(
		&self,
		account_id: i64,
		at: Timestamp,
		plan: &ErasurePlan,
	) -> ClResult<bool> {
		// `write_tx`, not `begin`: a deferred BEGIN reads first and cannot upgrade its lock,
		// so it fails `SQLITE_BUSY` on the spot however long `busy_timeout` is. An erasure
		// that half-applies is the worst possible outcome here.
		let mut tx = self.write_tx().await?;

		// Same predicate as `owned_org_tenants`, re-run inside the transaction: the service's
		// pre-check runs on the reader pool, so a `POST /api/tenants` between the two erased
		// the owner of a live organisation, which no route can repair.
		let owned: i64 = sqlx::query_scalar(
			"SELECT count(*) FROM tenants WHERE owner_account_id = ? AND kind != 'P'",
		)
		.bind(account_id)
		.fetch_one(&mut *tx)
		.await
		.db()?;
		if owned > 0 {
			return Ok(false);
		}

		// Read before the statement below destroys it: the address is the only handle on the
		// `SEND_EMAIL` payloads addressed to this person.
		let email: Option<String> = sqlx::query_scalar("SELECT email FROM accounts WHERE id = ?")
			.bind(account_id)
			.fetch_optional(&mut *tx)
			.await
			.db()?;

		// The placeholder keeps the UNIQUE index satisfied and is not reversible to the original
		// address; bumping `token_epoch` kills every live token. Built from `uid`, not `id`:
		// `members()` returns `accounts.email` for an accepted membership, so the internal
		// integer key leaked through `GET /api/tenant/members`.
		let mut set = set_clause(plan.accounts);
		if !set.is_empty() {
			set.push_str(", ");
		}
		let sql = format!(
			"UPDATE accounts \
			 SET {set}email = 'anonymized+' || uid || '@invalid', status = 'ANONYMIZED', \
				 anonymized_at = ?, token_epoch = token_epoch + 1 \
			 WHERE id = ?"
		);
		let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
		for (_, value) in plan.accounts {
			q = q.bind(*value);
		}
		q.bind(at.0).bind(account_id).execute(&mut *tx).await.db()?;

		for table in plan.delete_by_account {
			let sql = format!("DELETE FROM \"{table}\" WHERE account_id = ?");
			sqlx::query(sqlx::AssertSqlSafe(sql))
				.bind(account_id)
				.execute(&mut *tx)
				.await
				.db()?;
		}

		// `account_id`, not the tenant: a key belongs to the person, and a member's
		// organisation-scoped keys outlived their own erasure under the personal-tenant scope.
		sqlx::query(
			"UPDATE api_keys SET revoked_at = ? WHERE revoked_at IS NULL AND account_id = ?",
		)
		.bind(at.0)
		.bind(account_id)
		.execute(&mut *tx)
		.await
		.db()?;

		// Scoped to `kind = 'P'` for the same reason as the statements above and below — an
		// organisation this account merely owns keeps its trading name. Why the personal
		// tenant's name is personal data at all is `plan`'s to say.
		if !plan.tenants.is_empty() {
			let sql = format!(
				"UPDATE tenants SET {} WHERE owner_account_id = ? AND kind = 'P'",
				set_clause(plan.tenants)
			);
			let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
			for (_, value) in plan.tenants {
				q = q.bind(*value);
			}
			q.bind(account_id).execute(&mut *tx).await.db()?;
		}

		// The only cross-crate entry on the allowlist, doubly restricted.
		// `billing_parties.kind = 'P'` keeps it to natural persons; `tenants.kind = 'P'` keeps it
		// to the account's own personal tenant, because rows under an organisation it merely
		// owns are other people's data and erasing this account is not consent to destroy them.
		if !plan.billing_parties.is_empty() && has_table(&mut *tx, "billing_parties").await? {
			let sql = format!(
				"UPDATE billing_parties SET {} \
				 WHERE kind = 'P' \
				   AND tenant_id IN ( \
						SELECT id FROM tenants WHERE owner_account_id = ? AND kind = 'P' \
				   )",
				set_clause(plan.billing_parties)
			);
			let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
			for (_, value) in plan.billing_parties {
				q = q.bind(*value);
			}
			q.bind(account_id).execute(&mut *tx).await.db()?;
		}

		// Not restricted to FAILED: a PENDING row for an erased account must not be delivered
		// either. Matched on the JSON field rather than a LIKE so a name containing the address
		// cannot widen it, and the row itself stays — deleting it would perturb the runner.
		// `json_valid` is not redundant: `json_extract` on a blanked payload aborts the erasure.
		if let Some(email) = email {
			for kind in plan.blank_job_kinds {
				sqlx::query(
					"UPDATE jobs SET payload = '' \
					 WHERE kind = ? AND json_valid(payload) \
					   AND json_extract(payload, '$.to') = ?",
				)
				.bind(*kind)
				.bind(&email)
				.execute(&mut *tx)
				.await
				.db()?;
			}
		}

		tx.commit().await.db()?;
		Ok(true)
	}
}

// vim: ts=4
