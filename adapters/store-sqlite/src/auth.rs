// SPDX-License-Identifier: MPL-2.0
//! `AuthStore` over SQLite. Reads go through `reader()`, writes through `conn()`, and a
//! multi-statement write through `write_tx()` — all three answer with the bound transaction's
//! own connection when the handle has one.
//!
//! `mintworks-auth` carries no driver dependency, so no row type here can be decoded by derive:
//! every query binds primitives and every framework row is built by hand in the `*_row`
//! helpers below. See `util.rs` for the conversion vocabulary.

use async_trait::async_trait;
// The *same* function `gdpr::rescale` looks these keys back up with, not a second copy.
use mintworks_auth::gdpr::camel;
use mintworks_auth::store::{
	Account, AccountOrg, AccountStatus, ApiKey, AuthStore, Consent, ErasedCol, ErasurePlan,
	ExportScope, ExportSection, LegalDoc, LegalKind, Member, NewAccount, NewApiKey, NewConsent,
	NewLegalDoc, NewTotpCredential, NewWebauthnCredential, Org, OrgKind, OrgStatus, Role,
	TotpCredential, WebauthnCredential,
};
use mintworks_core::error::StatusCode;
use mintworks_core::prelude::*;
use mintworks_core::store::CoreStore;
use serde_json::Value;
use sqlx::{Row, SqliteConnection, sqlite::SqliteRow};

use crate::util::unique_as_conflict;
use crate::{
	SqliteStore,
	util::{DbExt, RowExt, RowsExt},
};

// ---------------------------------------------------------------- row mapping
//
// Every read below is **by column name**: `SELECT *`/`RETURNING *` follow the DDL's column order,
// which a migration may change, and three queries alias columns. By index it silently misaligns.

/// `accounts.is_operator` is gone: an operator is an `ADMIN`-or-`OWNER` on the root org. Every
/// `SELECT` feeding [`account_row`] aliases this in, so the flag still travels with the row.
const IS_ROOT_ADMIN: &str = "EXISTS (SELECT 1 FROM memberships m \
	   WHERE m.account_id = accounts.id \
	     AND m.org_id = (SELECT id FROM orgs WHERE kind = 'ROOT') \
	     AND m.role IN ('ADMIN', 'OWNER') AND m.accepted_at IS NOT NULL) AS is_root_admin";

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
		is_root_admin: row.try_get::<i64, _>("is_root_admin").db()? != 0,
		failed_logins: row.try_get("failed_logins").db()?,
		locked_until: row.try_get::<Option<i64>, _>("locked_until").db()?.map(Timestamp),
		activated_at: row.try_get::<Option<i64>, _>("activated_at").db()?.map(Timestamp),
		last_login_at: row.try_get::<Option<i64>, _>("last_login_at").db()?.map(Timestamp),
		anonymized_at: row.try_get::<Option<i64>, _>("anonymized_at").db()?.map(Timestamp),
		created_at: Timestamp(row.try_get("created_at").db()?),
	})
}

fn org_row(row: &SqliteRow) -> ClResult<Org> {
	Ok(Org {
		id: row.try_get("id").db()?,
		uid: OrgId::from_trusted(row.try_get::<String, _>("uid").db()?),
		kind: row.try_get::<String, _>("kind").db()?.parse()?,
		name: row.try_get("name").db()?,
		owner_account_id: row.try_get("owner_account_id").db()?,
		billing_currency: row
			.try_get::<Option<String>, _>("billing_currency")
			.db()?
			.map(CurrencyCode::from_trusted),
		status: row.try_get::<String, _>("status").db()?.parse()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
		slug: row.try_get("slug").db()?,
	})
}

fn account_org_row(row: &SqliteRow) -> ClResult<AccountOrg> {
	Ok(AccountOrg {
		uid: OrgId::from_trusted(row.try_get::<String, _>("uid").db()?),
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
		status: row.try_get::<String, _>("status").db()?.parse()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
	})
}

fn webauthn_row(row: &SqliteRow) -> ClResult<WebauthnCredential> {
	Ok(WebauthnCredential {
		id: row.try_get("id").db()?,
		account_id: row.try_get("account_id").db()?,
		credential_id: row.try_get("credential_id").db()?,
		credential: row.try_get("credential").db()?,
		name: row.try_get("name").db()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
		last_used_at: row.try_get::<Option<i64>, _>("last_used_at").db()?.map(Timestamp),
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
		org_uid: row.try_get::<Option<String>, _>("org_uid").db()?.map(OrgId::from_trusted),
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
/// closed allowlist `mintworks-auth` hands down (`mintworks_auth::gdpr::EXPORT`). **Which** columns
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
/// omission: `mintworks-auth` carries column names for tables it does not own, so drift has to be
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
		// `detail: None` — `ORG_DELETED`, all of `mintworks-invoice`'s — an unidentifiable stub.
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

/// Every section of the export, in order, on one connection.
async fn dump_sections(
	conn: &mut SqliteConnection,
	account_id: i64,
	sections: &[ExportSection],
) -> ClResult<Vec<Value>> {
	let mut out = Vec::with_capacity(sections.len());
	for section in sections {
		// `[]` rather than a failed export for a section with no columns yet, and for a table
		// this deployment does not have — `mintworks-invoice`'s, where it is not used.
		let rows = if section.columns.is_empty() || !has_table(&mut *conn, section.table).await? {
			Value::Array(Vec::new())
		} else {
			dump(
				conn,
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
	Ok(out)
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
/// `mintworks_auth::gdpr::EXPORT`'s; this is only their SQL.
fn where_of(scope: ExportScope) -> &'static str {
	match scope {
		ExportScope::Account => "id = ?",
		ExportScope::AccountId => "account_id = ?",
		ExportScope::MemberOrg => "id IN (SELECT org_id FROM memberships WHERE account_id = ?)",
		ExportScope::PersonalOrg => {
			"org_id IN (SELECT id FROM orgs WHERE owner_account_id = ? AND kind = 'PERSONAL')"
		}
		// Lines and VAT groups ship as sibling arrays rather than nested inside
		// each invoice — the same evidence, no join logic. Nest them if a reader needs it.
		ExportScope::PersonalOrgInvoice => {
			"invoice_id IN (SELECT id FROM invoices WHERE \
			 org_id IN (SELECT id FROM orgs WHERE owner_account_id = ? AND kind = 'PERSONAL'))"
		}
	}
}

/// `"a" = ?, "b" = ?` for an erasure allowlist, empty when the allowlist is. Column names
/// come from `mintworks_auth::gdpr::ERASURE`, a `&'static` value in the feature crate and never
/// from a request; the replacement literals are **bound**, in the same order.
fn set_clause(cols: &[ErasedCol]) -> String {
	cols.iter()
		.map(|(col, _)| format!("\"{col}\" = ?"))
		.collect::<Vec<_>>()
		.join(", ")
}

/// `mintworks-invoice`'s tables are absent in a deployment that does not use it.
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
	) -> ClResult<(Account, Org)> {
		let now = Timestamp::now();
		let tx = self.write_tx().await?;

		// Bound to a `let` so the borrow in `.bind(uid.as_str())` outlives the statement.
		let account_uid = AccountId::generate();
		let row = sqlx::query(
			"INSERT INTO accounts (uid, email, pwd_hash, name, locale, created_at)
			 VALUES (?, ?, ?, ?, ?, ?) RETURNING *, 0 AS is_root_admin",
		)
		.bind(account_uid.as_str())
		.bind(&new.email)
		.bind(&new.pwd_hash)
		.bind(&new.name)
		.bind(&new.locale)
		.bind(now.0)
		.fetch_one(&mut *tx.lock().await?)
		.await
		.map_err(|e| unique_as_conflict(&e, "email already registered"))?;
		let account = account_row(&row)?;

		let org_uid = OrgId::generate();
		let row = sqlx::query(
			"INSERT INTO orgs (uid, parent_id, kind, name, owner_account_id, created_at)
			 VALUES (?, (SELECT id FROM orgs WHERE kind = 'ROOT'), 'PERSONAL', ?, ?, ?)
			 RETURNING *",
		)
		.bind(org_uid.as_str())
		.bind(&new.org_name)
		.bind(account.id)
		.bind(now.0)
		.fetch_one(&mut *tx.lock().await?)
		.await
		.db()?;
		let org = org_row(&row)?;

		// The owner does not invite themselves, so this membership is accepted on creation.
		// Without `accepted_at`, `mintworks_auth::token::pick_org` skips it and login mints a
		// token with no `org` claim.
		sqlx::query(
			"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
			 VALUES (?, ?, 'OWNER', ?, ?)",
		)
		.bind(org.id)
		.bind(account.id)
		.bind(now.0)
		.bind(now.0)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;

		// In the same transaction as the account: written outside it, a `SQLITE_BUSY` left an
		// account with no ToS/privacy rows, which `consent::gate` then blocks on every gated
		// route — unusable and unrepairable. `account_id` comes from the row just inserted.
		for c in consents {
			sqlx::query(
				"INSERT INTO consents
					(account_id, org_id, kind, legal_doc_id, doc_version, doc_sha256,
					 granted, at, ip, user_agent)
				 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
			)
			.bind(account.id)
			.bind(c.org_id)
			.bind(c.kind.as_str())
			.bind(c.legal_doc_id)
			.bind(&c.doc_version)
			.bind(&c.doc_sha256)
			.bind(c.granted)
			.bind(now.0)
			.bind(&c.ip)
			.bind(&c.user_agent)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;
		}

		tx.commit().await?;
		Ok((account, org))
	}

	async fn account_by_email(&self, email: &str) -> ClResult<Option<Account>> {
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"SELECT *, {IS_ROOT_ADMIN} FROM accounts WHERE email = ?"
		)))
		.bind(email)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.one(account_row)
	}

	async fn account_by_uid(&self, uid: &AccountId) -> ClResult<Option<Account>> {
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"SELECT *, {IS_ROOT_ADMIN} FROM accounts WHERE uid = ?"
		)))
		.bind(uid.as_str())
		.fetch_optional(&mut *self.reader().await?)
		.await
		.one(account_row)
	}

	async fn account_by_id(&self, id: i64) -> ClResult<Option<Account>> {
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"SELECT *, {IS_ROOT_ADMIN} FROM accounts WHERE id = ?"
		)))
		.bind(id)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.one(account_row)
	}

	async fn pending_ref(&self, id: i64) -> ClResult<Option<mintworks_core::ids::RefId>> {
		let uid: Option<String> = sqlx::query_scalar(
			"SELECT r.uid FROM accounts a JOIN refs r ON r.id = a.pending_ref_id WHERE a.id = ?",
		)
		.bind(id)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.db()?;
		Ok(uid.map(mintworks_core::ids::RefId::from_trusted))
	}

	async fn set_pending_ref(&self, id: i64, ref_id: i64) -> ClResult<bool> {
		let res = sqlx::query(
			"UPDATE accounts SET pending_ref_id = ?
			 WHERE id = ? AND status = 'PENDING' AND pending_ref_id IS NULL",
		)
		.bind(ref_id)
		.bind(id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
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
		.execute(&mut *self.conn().await?)
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
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn bump_token_epoch(&self, id: i64) -> ClResult<()> {
		sqlx::query("UPDATE accounts SET token_epoch = token_epoch + 1 WHERE id = ?")
			.bind(id)
			.execute(&mut *self.conn().await?)
			.await
			.db()?;
		Ok(())
	}

	/// The `WHERE` predicate is the whole guarantee that GDPR erasure is irreversible: without it
	/// an operator un-suspending a batch — or a consumer calling the trait directly — moves an
	/// anonymized account (`email = 'anonymized+…@invalid'`, `pwd_hash = NULL`) back to `ACTIVE`,
	/// which `auth_mw::account_for_token` then accepts. Hardcoded in the SQL like
	/// `InvoiceStore::mark_paid`/`mark_stornoed`; a consumer's own raw SQL is its own
	/// responsibility.
	async fn set_account_status(&self, id: i64, status: AccountStatus) -> ClResult<()> {
		// `write_tx` for the same reason as `anonymize_account`: the epoch bump is the whole
		// point of a suspension, and as a second connection's write it could fail alone and
		// leave the account SUSPENDED with every issued token still live.
		let tx = self.write_tx().await?;
		let res = sqlx::query(
			"UPDATE accounts SET status = ?
			  WHERE id = ? AND (status <> 'ANONYMIZED' OR ? = 'ANONYMIZED')",
		)
		.bind(status.as_str())
		.bind(id)
		.bind(status.as_str())
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
		if res.rows_affected() == 0 {
			// Zero rows is either no such account — which the trait's `ClResult<()>` has
			// always reported as nothing to do — or the one case worth naming. One extra read,
			// on a path that is already an error.
			let current: Option<String> =
				sqlx::query_scalar("SELECT status FROM accounts WHERE id = ?")
					.bind(id)
					.fetch_optional(&mut *tx.lock().await?)
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
				.execute(&mut *tx.lock().await?)
				.await
				.db()?;
		}
		tx.commit().await?;
		Ok(())
	}

	async fn record_login_failure(&self, id: i64) -> ClResult<()> {
		// Unconditional, and it must stay that way: `id` is `login::NO_ACCOUNT` on the
		// unknown-address branch, where matching no row is the point and paying the writer
		// round trip anyway is what keeps the branch indistinguishable.
		sqlx::query("UPDATE accounts SET failed_logins = failed_logins + 1 WHERE id = ?")
			.bind(id)
			.execute(&mut *self.conn().await?)
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
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(())
	}

	// -- orgs

	async fn create_org(
		&self,
		kind: OrgKind,
		parent_id: i64,
		name: &str,
		owner_account_id: i64,
		billing_currency: Option<&CurrencyCode>,
	) -> ClResult<Org> {
		let now = Timestamp::now();
		// `write_tx` for the same reason as `anonymize_account`: a deferred BEGIN cannot
		// upgrade its lock and fails `SQLITE_BUSY` outright under a second writer.
		let tx = self.write_tx().await?;

		let org_uid = OrgId::generate();
		let row = sqlx::query(
			"INSERT INTO orgs (uid, parent_id, kind, name, owner_account_id, billing_currency,
			 created_at) VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING *",
		)
		.bind(org_uid.as_str())
		.bind(parent_id)
		.bind(kind.as_str())
		.bind(name)
		.bind(owner_account_id)
		.bind(billing_currency.map(CurrencyCode::as_str))
		.bind(now.0)
		.fetch_one(&mut *tx.lock().await?)
		.await
		.map_err(|e| unique_as_conflict(&e, "this account already has an org of that kind"))?;
		let org = org_row(&row)?;

		// The owner does not invite themselves, so this membership is accepted on creation.
		sqlx::query(
			"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
			 VALUES (?, ?, 'OWNER', ?, ?)",
		)
		.bind(org.id)
		.bind(owner_account_id)
		.bind(now.0)
		.bind(now.0)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;

		tx.commit().await?;
		Ok(org)
	}

	async fn org_by_uid(&self, uid: &OrgId) -> ClResult<Option<Org>> {
		sqlx::query("SELECT * FROM orgs WHERE uid = ?")
			.bind(uid.as_str())
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(org_row)
	}

	async fn org_by_id(&self, id: i64) -> ClResult<Option<Org>> {
		sqlx::query("SELECT * FROM orgs WHERE id = ?")
			.bind(id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(org_row)
	}

	async fn update_org(
		&self,
		id: i64,
		name: Option<&str>,
		billing_currency: Patch<CurrencyCode>,
		status: Option<OrgStatus>,
	) -> ClResult<()> {
		// Only the status needs the probe: renaming the root is harmless, suspending it is not.
		if status == Some(OrgStatus::Suspended) {
			// The ancestor walks anchor on `status = 'ACTIVE'`, so a suspended root strips every
			// inherited role — including the operator authority that is the only way to un-suspend it.
			// The probe and the update share one `BEGIN IMMEDIATE`, or a concurrent reparent lands
			// between them.
			let tx = self.write_tx().await?;
			let is_root: i64 = sqlx::query_scalar::<_, i64>(
				"SELECT count(*) FROM orgs WHERE id = ? AND kind = 'ROOT'",
			)
			.bind(id)
			.fetch_one(&mut *tx.lock().await?)
			.await
			.db()?;
			if is_root > 0 {
				return Err(Error::conflict("the platform root org cannot be suspended"));
			}
			write_org(&mut *tx.lock().await?, id, name, billing_currency, status).await?;
			tx.commit().await?;
			return Ok(());
		}
		// A rename takes no lock beyond this statement's own, or every plain `PATCH /api/org`
		// would serialise against every other writer on the single writer connection.
		let mut conn = self.conn().await?;
		write_org(&mut conn, id, name, billing_currency, status).await
	}

	async fn set_org_slug(&self, id: i64, slug: Option<&str>) -> ClResult<()> {
		sqlx::query("UPDATE orgs SET slug = ? WHERE id = ?")
			.bind(slug)
			.bind(id)
			.execute(&mut *self.conn().await?)
			.await
			.map_err(|e| match &e {
				sqlx::Error::Database(db) if db.is_unique_violation() => {
					mintworks_core::refs::slug_taken()
				}
				_ => crate::util::map_db(&e),
			})?;
		Ok(())
	}

	async fn transfer_org_ownership(&self, org_id: i64, from: i64, to: i64) -> ClResult<bool> {
		let tx = self.write_tx().await?;

		// Both predicates re-run inside the transaction, as `anonymize_account` re-runs its
		// own: the service checked them on the reader pool, where a concurrent `remove_member`
		// is invisible.
		let ok: Option<i64> = sqlx::query_scalar(
			"SELECT 1 FROM orgs t \
			  JOIN memberships m ON m.org_id = t.id AND m.account_id = ? \
			 WHERE t.id = ? AND t.owner_account_id = ? AND m.accepted_at IS NOT NULL",
		)
		.bind(to)
		.bind(org_id)
		.bind(from)
		.fetch_optional(&mut *tx.lock().await?)
		.await
		.db()?;
		if ok.is_none() {
			return Ok(false);
		}

		// Demoted to `ADMIN`, not removed: an owner handing the organisation over is still a
		// member of it, and `remove_member` is the separate decision.
		for (account_id, role) in [(from, "ADMIN"), (to, "OWNER")] {
			sqlx::query("UPDATE memberships SET role = ? WHERE org_id = ? AND account_id = ?")
				.bind(role)
				.bind(org_id)
				.bind(account_id)
				.execute(&mut *tx.lock().await?)
				.await
				.db()?;
		}
		sqlx::query("UPDATE orgs SET owner_account_id = ? WHERE id = ?")
			.bind(to)
			.bind(org_id)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;

		tx.commit().await?;
		Ok(true)
	}

	async fn delete_org(&self, org_id: i64) -> ClResult<bool> {
		let tx = self.write_tx().await?;

		let others: i64 = sqlx::query_scalar(
			"SELECT count(*) FROM memberships m JOIN orgs t ON t.id = m.org_id \
			  WHERE m.org_id = ? AND m.accepted_at IS NOT NULL \
				AND m.account_id IS NOT t.owner_account_id",
		)
		.bind(org_id)
		.fetch_one(&mut *tx.lock().await?)
		.await
		.db()?;
		if others > 0 {
			return Ok(false);
		}

		// Every non-cascading `REFERENCES orgs(id)`: `invoices` under the eight-year Hungarian
		// retention obligation, `consents` as evidence, `sellers` for the taxpayer id and the
		// `doc_series` counter behind issued numbers, plus `services`, `payments` and child orgs.
		// `objects` and `documents` are here *because* they cascade: the rows would go with no
		// check and no trace, and a document's file with no row left to find it by.
		// `subscriptions` and `offers` hold the plan history and a seller's catalogue.
		// `has_table` per table, not one merged count: a deployment without `mintworks-invoice` or
		// `mintworks-billing` has no such table and the merged statement failed as a 500.
		for (table, column) in [
			("subscriptions", "org_id"),
			("offers", "seller_org_id"),
			("invoices", "org_id"),
			("consents", "org_id"),
			("sellers", "org_id"),
			("services", "org_id"),
			("payments", "org_id"),
			("objects", "org_id"),
			("documents", "org_id"),
			("orgs", "parent_id"),
		] {
			if !has_table(&mut *tx.lock().await?, table).await? {
				continue;
			}
			// `table` and `column` are literals from the array above, never caller input.
			let kept: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
				"SELECT count(*) FROM {table} WHERE {column} = ?"
			)))
			.bind(org_id)
			.fetch_one(&mut *tx.lock().await?)
			.await
			.db()?;
			if kept > 0 {
				return Ok(false);
			}
		}

		let deletable: i64 = sqlx::query_scalar(
			"SELECT count(*) FROM orgs WHERE id = ? AND kind NOT IN ('PERSONAL','ROOT')",
		)
		.bind(org_id)
		.fetch_one(&mut *tx.lock().await?)
		.await
		.db()?;
		if deletable == 0 {
			return Ok(false);
		}
		// `refs.org_id` does not cascade and every invite mints one. Coupon refs are a seller's,
		// and a seller org is kept by `sellers`/`services`/`offers` above.
		for sql in [
			"UPDATE accounts SET pending_ref_id = NULL \
			  WHERE pending_ref_id IN (SELECT id FROM refs WHERE org_id = ?)",
			"DELETE FROM ref_uses WHERE ref_id IN (SELECT id FROM refs WHERE org_id = ?)",
			"DELETE FROM refs WHERE org_id = ?",
		] {
			sqlx::query(sql).bind(org_id).execute(&mut *tx.lock().await?).await.db()?;
		}

		let gone = sqlx::query("DELETE FROM orgs WHERE id = ? AND kind NOT IN ('PERSONAL','ROOT')")
			.bind(org_id)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;
		// FK-less, so nothing cascades: the org's runs and their events go with it. `llm_usage`
		// stays, it is the cost ledger.
		if gone.rows_affected() > 0 {
			sqlx::query("DELETE FROM agent_runs WHERE org_id = ?")
				.bind(org_id)
				.execute(&mut *tx.lock().await?)
				.await
				.db()?;
		}
		tx.commit().await?;
		Ok(gone.rows_affected() > 0)
	}

	async fn orgs_for_account(&self, account_id: i64) -> ClResult<Vec<AccountOrg>> {
		sqlx::query(
			"SELECT t.uid AS uid, t.kind AS kind, t.name AS name, t.status AS status,
					m.role AS role, m.accepted_at AS accepted_at
			 FROM memberships m JOIN orgs t ON t.id = m.org_id
			 WHERE m.account_id = ? ORDER BY t.created_at",
		)
		.bind(account_id)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(account_org_row)
	}

	async fn owned_shared_orgs(&self, account_id: i64) -> ClResult<Vec<Org>> {
		sqlx::query(
			"SELECT * FROM orgs WHERE owner_account_id = ? AND kind != 'PERSONAL' ORDER BY id",
		)
		.bind(account_id)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(org_row)
	}

	async fn currency_enabled(&self, code: &CurrencyCode) -> ClResult<bool> {
		if !has_table(&mut *self.reader().await?, "currencies").await? {
			return Ok(true);
		}
		let n: i64 =
			sqlx::query_scalar("SELECT count(*) FROM currencies WHERE code = ? AND enabled = 1")
				.bind(code.as_str())
				.fetch_one(&mut *self.reader().await?)
				.await
				.db()?;
		Ok(n > 0)
	}

	// -- memberships

	async fn membership_role(&self, org_id: i64, account_id: i64) -> ClResult<Option<Role>> {
		sqlx::query_scalar::<_, String>(
			"SELECT role FROM memberships WHERE org_id = ? AND account_id = ?",
		)
		.bind(org_id)
		.bind(account_id)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.db()?
		.map(|s| s.parse())
		.transpose()
	}

	async fn accepted_membership_role(
		&self,
		org_id: i64,
		account_id: i64,
	) -> ClResult<Option<Role>> {
		sqlx::query_scalar::<_, String>(
			"SELECT role FROM memberships
			 WHERE org_id = ? AND account_id = ? AND accepted_at IS NOT NULL",
		)
		.bind(org_id)
		.bind(account_id)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.db()?
		.map(|s| s.parse())
		.transpose()
	}

	async fn membership_created_at(
		&self,
		org_id: i64,
		account_id: i64,
	) -> ClResult<Option<Timestamp>> {
		sqlx::query_scalar::<_, i64>(
			"SELECT created_at FROM memberships WHERE org_id = ? AND account_id = ?",
		)
		.bind(org_id)
		.bind(account_id)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.db()
		.map(|o| o.map(Timestamp))
	}

	async fn accept_membership(&self, org_id: i64, account_id: i64, at: Timestamp) -> ClResult<()> {
		sqlx::query(
			"UPDATE memberships SET accepted_at = ?
			 WHERE org_id = ? AND account_id = ? AND accepted_at IS NULL",
		)
		.bind(at.0)
		.bind(org_id)
		.bind(account_id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(())
	}

	async fn put_membership(&self, org_id: i64, account_id: i64, role: Role) -> ClResult<bool> {
		// `memberships.role <> 'OWNER'` in the statement, not just in the service: the service
		// checks on the reader pool, where a concurrent `transfer_org_ownership` is
		// invisible. Guards only the update branch, so org creation still inserts an OWNER.
		let res = sqlx::query(
			"INSERT INTO memberships (org_id, account_id, role, created_at)
			 VALUES (?, ?, ?, ?)
			 ON CONFLICT (org_id, account_id) DO UPDATE SET role = excluded.role
			   WHERE memberships.role <> 'OWNER'",
		)
		.bind(org_id)
		.bind(account_id)
		.bind(role.as_str())
		.bind(Timestamp::now().0)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn remove_membership(&self, org_id: i64, account_id: i64) -> ClResult<bool> {
		// No `token_epoch` bump: the epoch is account-wide, so one org's admin would sign the
		// account out of every other org. The deleted row suffices — `auth_mw`'s org join
		// needs a live membership. `role <> 'OWNER'` in the statement, not just in the service:
		// the service checks on the reader pool, where a concurrent transfer is invisible.
		let res = sqlx::query(
			"DELETE FROM memberships WHERE org_id = ? AND account_id = ? AND role <> 'OWNER'",
		)
		.bind(org_id)
		.bind(account_id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn members(&self, org_id: i64, limit: i64) -> ClResult<Vec<Member>> {
		// Accepted rows only, and the `WHERE` is the whole filter.
		sqlx::query(
			"SELECT a.uid AS account_uid, a.email AS email, a.name AS name, m.role AS role,
					a.status AS status, m.created_at AS created_at
			 FROM memberships m JOIN accounts a ON a.id = m.account_id
			 WHERE m.org_id = ? AND m.accepted_at IS NOT NULL ORDER BY m.created_at LIMIT ?",
		)
		.bind(org_id)
		.bind(limit)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(member_row)
	}

	// -- api keys

	async fn create_api_key(&self, new: &NewApiKey, max_live: i64) -> ClResult<Option<ApiKey>> {
		let uid = ApiKeyId::generate();
		let now = Timestamp::now();
		// The cap rides in the `WHERE` of the `INSERT … SELECT`: `sqlite` serialises writers, so
		// the count and the row land in one statement and two concurrent mints cannot both
		// observe room below the cap. An expired key is not live, so it does not count.
		let inserted: Option<i64> = sqlx::query_scalar(
			"INSERT INTO api_keys
				(uid, org_id, account_id, name, prefix, key_hash, scopes, expires_at,
				 created_at)
			 SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?
			  WHERE (SELECT count(*) FROM api_keys
			          WHERE org_id = ? AND revoked_at IS NULL
			            AND (expires_at IS NULL OR expires_at > ?)) < ?
			 RETURNING id",
		)
		.bind(uid.as_str())
		.bind(new.org_id)
		.bind(new.account_id)
		.bind(&new.name)
		.bind(&new.prefix)
		.bind(&new.key_hash)
		.bind(&new.scopes)
		.bind(new.expires_at.map(|t| t.0))
		.bind(now.0)
		.bind(new.org_id)
		.bind(now.0)
		.bind(max_live)
		.fetch_optional(&mut *self.conn().await?)
		.await
		.map_err(|e| unique_as_conflict(&e, "api key prefix collision"))?;
		if inserted.is_none() {
			return Ok(None);
		}
		// Read the row back rather than mapping the insert: one type for every key read, and
		// `prefix` is UNIQUE, so a concurrent mint cannot hand back someone else's row.
		let row = self
			.api_key_by_prefix(&new.prefix)
			.await?
			.ok_or_else(|| Error::internal("api key vanished after insert"))?;
		Ok(Some(row))
	}

	async fn api_keys_for_org(&self, org_id: i64) -> ClResult<Vec<ApiKey>> {
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"{} WHERE k.org_id = ? ORDER BY k.created_at",
			crate::core::api_key_select()
		)))
		.bind(org_id)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(crate::core::api_key_row)
	}

	async fn revoke_api_key(&self, org_id: i64, uid: &ApiKeyId, at: Timestamp) -> ClResult<bool> {
		let res = sqlx::query(
			"UPDATE api_keys SET revoked_at = ?
			  WHERE uid = ? AND org_id = ? AND revoked_at IS NULL",
		)
		.bind(at.0)
		.bind(uid.as_str())
		.bind(org_id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn rename_api_key(&self, org_id: i64, uid: &ApiKeyId, name: &str) -> ClResult<bool> {
		let res = sqlx::query("UPDATE api_keys SET name = ? WHERE uid = ? AND org_id = ?")
			.bind(name)
			.bind(uid.as_str())
			.bind(org_id)
			.execute(&mut *self.conn().await?)
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
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(res.rows_affected() > 0)
	}

	async fn totp_by_account(&self, account_id: i64) -> ClResult<Option<TotpCredential>> {
		sqlx::query("SELECT * FROM totp_credentials WHERE account_id = ?")
			.bind(account_id)
			.fetch_optional(&mut *self.reader().await?)
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
		.execute(&mut *self.conn().await?)
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
		.execute(&mut *self.conn().await?)
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
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn delete_totp(&self, account_id: i64) -> ClResult<bool> {
		let res = sqlx::query("DELETE FROM totp_credentials WHERE account_id = ?")
			.bind(account_id)
			.execute(&mut *self.conn().await?)
			.await
			.db()?;
		Ok(res.rows_affected() == 1)
	}

	// -- webauthn credentials (passkeys)

	async fn put_webauthn_credential(
		&self,
		new: &NewWebauthnCredential,
		max: i64,
	) -> ClResult<Option<WebauthnCredential>> {
		let row = sqlx::query(
			"INSERT INTO webauthn_credentials
				(account_id, credential_id, credential, name, created_at)
			 SELECT ?, ?, ?, ?, ?
			  WHERE (SELECT count(*) FROM webauthn_credentials WHERE account_id = ?) < ?
			 RETURNING *",
		)
		.bind(new.account_id)
		.bind(&new.credential_id)
		.bind(&new.credential)
		.bind(&new.name)
		.bind(new.created_at.0)
		.bind(new.account_id)
		.bind(max)
		.fetch_optional(&mut *self.conn().await?)
		.await
		.map_err(|e| unique_as_conflict(&e, "credential already registered"))?;
		// No row is the cap, not an error: the count and the insert are one statement.
		row.map(|r| webauthn_row(&r)).transpose()
	}

	/// The usernameless-login lookup: an assertion names only the credential it used, so this runs
	/// before any account is in hand.
	async fn webauthn_by_credential_id(
		&self,
		credential_id: &str,
	) -> ClResult<Option<WebauthnCredential>> {
		sqlx::query("SELECT * FROM webauthn_credentials WHERE credential_id = ?")
			.bind(credential_id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(webauthn_row)
	}

	async fn webauthn_for_account(&self, account_id: i64) -> ClResult<Vec<WebauthnCredential>> {
		sqlx::query("SELECT * FROM webauthn_credentials WHERE account_id = ? ORDER BY created_at")
			.bind(account_id)
			.fetch_all(&mut *self.reader().await?)
			.await
			.all(webauthn_row)
	}

	async fn rename_webauthn(
		&self,
		account_id: i64,
		credential_id: &str,
		name: &str,
	) -> ClResult<bool> {
		let res = sqlx::query(
			"UPDATE webauthn_credentials SET name = ? WHERE credential_id = ? AND account_id = ?",
		)
		.bind(name)
		.bind(credential_id)
		.bind(account_id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn delete_webauthn(&self, account_id: i64, credential_id: &str) -> ClResult<bool> {
		let res = sqlx::query(
			"DELETE FROM webauthn_credentials WHERE credential_id = ? AND account_id = ?",
		)
		.bind(credential_id)
		.bind(account_id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn touch_webauthn(
		&self,
		credential_id: &str,
		credential: &str,
		at: Timestamp,
	) -> ClResult<()> {
		sqlx::query(
			"UPDATE webauthn_credentials SET credential = ?, last_used_at = ?
			  WHERE credential_id = ?",
		)
		.bind(credential)
		.bind(at.0)
		.bind(credential_id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(())
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
		.fetch_one(&mut *self.conn().await?)
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
		.fetch_optional(&mut *self.reader().await?)
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
		.fetch_optional(&mut *self.reader().await?)
		.await
		.one(legal_doc_row)
	}

	async fn record_consent(&self, new: &NewConsent, at: Timestamp) -> ClResult<i64> {
		Ok(sqlx::query_scalar(
			"INSERT INTO consents
				(account_id, org_id, kind, legal_doc_id, doc_version, doc_sha256, granted,
				 at, ip, user_agent)
			 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
		)
		.bind(new.account_id)
		.bind(new.org_id)
		.bind(new.kind.as_str())
		.bind(new.legal_doc_id)
		.bind(&new.doc_version)
		.bind(&new.doc_sha256)
		.bind(new.granted)
		.bind(at.0)
		.bind(&new.ip)
		.bind(&new.user_agent)
		.fetch_one(&mut *self.conn().await?)
		.await
		.db()?)
	}

	/// `AND c.org_id IS ?`, not `= ?`: in SQLite `IS` compares NULL correctly and behaves
	/// as `=` for everything else, so one bound parameter serves both the account-wide scope
	/// and an org-scoped one.
	///
	/// Ordered by `c.id` alone: leading with `c.at` disagreed with `list_consents`' `MAX(id)`
	/// across a clock step back, so the listing showed one row and the withdrawal took another.
	async fn latest_consent(
		&self,
		account_id: i64,
		kind: LegalKind,
		org_id: Option<i64>,
	) -> ClResult<Option<Consent>> {
		sqlx::query(
			"SELECT c.*, t.uid AS org_uid FROM consents c
			 LEFT JOIN orgs t ON t.id = c.org_id
			 WHERE c.account_id = ? AND c.kind = ? AND c.org_id IS ?
			 ORDER BY c.id DESC LIMIT 1",
		)
		.bind(account_id)
		.bind(kind.as_str())
		.bind(org_id)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.one(consent_row)
	}

	/// The newest row per `(kind, org_id)`. `MAX(c.id)` picks it — `consents` is
	/// append-only and `id` is monotonic, so the newest id in a group *is* its newest row,
	/// and SQLite's bare-column rule then yields that row's other columns.
	async fn list_consents(&self, account_id: i64) -> ClResult<Vec<Consent>> {
		sqlx::query(
			"SELECT c.*, MAX(c.id) AS newest, t.uid AS org_uid FROM consents c
			 LEFT JOIN orgs t ON t.id = c.org_id
			 WHERE c.account_id = ?
			 GROUP BY c.kind, c.org_id
			 ORDER BY c.kind, c.org_id",
		)
		.bind(account_id)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(consent_row)
	}

	async fn withdraw_consent(&self, id: i64, at: Timestamp) -> ClResult<bool> {
		let res = sqlx::query(
			"UPDATE consents SET withdrawn_at = ? WHERE id = ? AND withdrawn_at IS NULL",
		)
		.bind(at.0)
		.bind(id)
		.execute(&mut *self.conn().await?)
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
		// consistent" is the wrong bar.
		// A bound handle is already inside the caller's transaction, which is both the snapshot to
		// read and a `BEGIN` that would fail. Safe to hold the guard across `dump_sections`:
		// nothing inside takes the handle again.
		if let Some(held) = self.conn.scope()? {
			let mut conn = held.lock_conn().await?;
			return dump_sections(&mut conn, account_id, sections).await;
		}
		// A real `Transaction`, not a raw `BEGIN`: it rolls back on drop, so a cancelled export
		// cannot hand the reader pool a connection with a read transaction open. Deferred — a WAL
		// reader blocks nobody.
		let mut tx = self.read_pool().begin().await.db()?;
		dump_sections(&mut tx, account_id, sections).await
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
		let tx = self.write_tx().await?;

		// Same predicate as `owned_shared_orgs`, re-run inside the transaction: the service's
		// pre-check runs on the reader pool, so a `POST /api/orgs` between the two erased
		// the owner of a live organisation, which no route can repair.
		let owned: i64 = sqlx::query_scalar(
			"SELECT count(*) FROM orgs WHERE owner_account_id = ? AND kind != 'PERSONAL'",
		)
		.bind(account_id)
		.fetch_one(&mut *tx.lock().await?)
		.await
		.db()?;
		if owned > 0 {
			return Ok(false);
		}

		// Read before the statement below destroys it: the address is the only handle on the
		// `SEND_EMAIL` payloads addressed to this person.
		let email: Option<String> = sqlx::query_scalar("SELECT email FROM accounts WHERE id = ?")
			.bind(account_id)
			.fetch_optional(&mut *tx.lock().await?)
			.await
			.db()?;

		// The placeholder keeps the UNIQUE index satisfied and is not reversible to the original
		// address; bumping `token_epoch` kills every live token. Built from `uid`, not `id`:
		// `members()` returns `accounts.email` for an accepted membership, so the internal
		// integer key leaked through `GET /api/org/members`.
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
		q.bind(at.0).bind(account_id).execute(&mut *tx.lock().await?).await.db()?;

		let set = set_clause(plan.agent_runs);
		if !set.is_empty() {
			// Before the UPDATE: it nulls `account_id`, the only handle on the account's runs.
			sqlx::query(
				"DELETE FROM agent_run_events
				 WHERE run_id IN (SELECT id FROM agent_runs WHERE account_id = ?)",
			)
			.bind(account_id)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;
			let sql = format!("UPDATE agent_runs SET {set} WHERE account_id = ?");
			let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
			for (_, value) in plan.agent_runs {
				q = q.bind(*value);
			}
			q.bind(account_id).execute(&mut *tx.lock().await?).await.db()?;
		}

		for table in plan.delete_by_account {
			let sql = format!("DELETE FROM \"{table}\" WHERE account_id = ?");
			sqlx::query(sqlx::AssertSqlSafe(sql))
				.bind(account_id)
				.execute(&mut *tx.lock().await?)
				.await
				.db()?;
		}

		// `account_id`, not the org: a key belongs to the person, and a member's
		// organisation-scoped keys outlived their own erasure under the personal-org scope.
		sqlx::query(
			"UPDATE api_keys SET revoked_at = ? WHERE revoked_at IS NULL AND account_id = ?",
		)
		.bind(at.0)
		.bind(account_id)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;

		// Scoped to `kind = 'PERSONAL'` for the same reason as the statements above and below — an
		// organisation this account merely owns keeps its trading name. Why the personal
		// org's name is personal data at all is `plan`'s to say.
		if !plan.orgs.is_empty() {
			let sql = format!(
				"UPDATE orgs SET {} WHERE owner_account_id = ? AND kind = 'PERSONAL'",
				set_clause(plan.orgs)
			);
			let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
			for (_, value) in plan.orgs {
				q = q.bind(*value);
			}
			q.bind(account_id).execute(&mut *tx.lock().await?).await.db()?;
		}

		// The only cross-crate entry on the allowlist, doubly restricted.
		// `billing_parties.kind = 'PERSONAL'` keeps it to natural persons; `orgs.kind = 'PERSONAL'` keeps it
		// to the account's own personal org, because rows under an organisation it merely
		// owns are other people's data and erasing this account is not consent to destroy them.
		if !plan.billing_parties.is_empty()
			&& has_table(&mut *tx.lock().await?, "billing_parties").await?
		{
			let sql = format!(
				"UPDATE billing_parties SET {} \
				 WHERE kind = 'P' \
				   AND org_id IN ( \
						SELECT id FROM orgs WHERE owner_account_id = ? AND kind = 'PERSONAL' \
				   )",
				set_clause(plan.billing_parties)
			);
			let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
			for (_, value) in plan.billing_parties {
				q = q.bind(*value);
			}
			q.bind(account_id).execute(&mut *tx.lock().await?).await.db()?;
		}

		// `objects` has no account column, so the account's personal org is the only handle on its
		// ext blobs — the same scope as the two `UPDATE`s above, and the only path that reaches a
		// `PERSONAL` org's objects at all. The row stays and only the body is blanked, because
		// erasure here is anonymization; `object_index` is derived from `body`, so its rows go in
		// the same transaction rather than answering later from a value the body has dropped.
		if !plan.objects.is_empty() {
			const PERSONAL: &str = "org_id IN (SELECT id FROM orgs \
			                         WHERE owner_account_id = ? AND kind = 'PERSONAL')";
			let sql = format!("UPDATE objects SET {} WHERE {PERSONAL}", set_clause(plan.objects));
			let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
			for (_, value) in plan.objects {
				q = q.bind(*value);
			}
			q.bind(account_id).execute(&mut *tx.lock().await?).await.db()?;

			let sql = format!(
				"DELETE FROM object_index \
				  WHERE object_id IN (SELECT id FROM objects WHERE {PERSONAL})"
			);
			sqlx::query(sqlx::AssertSqlSafe(sql))
				.bind(account_id)
				.execute(&mut *tx.lock().await?)
				.await
				.db()?;
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
				.execute(&mut *tx.lock().await?)
				.await
				.db()?;
			}
			// The placeholder, not NULL: a NULL `refs.email` means anyone may use the ref.
			sqlx::query(
				"UPDATE refs SET email = (SELECT email FROM accounts WHERE id = ?) WHERE email = ?",
			)
			.bind(account_id)
			.bind(&email)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;
		}

		tx.commit().await?;
		Ok(true)
	}
}

/// The column-wise `UPDATE` [`AuthStore::update_org`] runs, on whichever handle the caller holds.
async fn write_org(
	conn: &mut SqliteConnection,
	id: i64,
	name: Option<&str>,
	billing_currency: Patch<CurrencyCode>,
	status: Option<OrgStatus>,
) -> ClResult<()> {
	let currency = billing_currency.as_option();
	sqlx::query(
		"UPDATE orgs SET
			name = COALESCE(?, name),
			billing_currency = CASE WHEN ? THEN ? ELSE billing_currency END,
			status = COALESCE(?, status)
		 WHERE id = ?",
	)
	.bind(name)
	.bind(currency.is_some())
	.bind(currency.flatten().map(CurrencyCode::as_str))
	.bind(status.map(OrgStatus::as_str))
	.bind(id)
	.execute(&mut *conn)
	.await
	.db()?;
	Ok(())
}

// vim: ts=4
