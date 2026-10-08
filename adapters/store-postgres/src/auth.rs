// SPDX-License-Identifier: MPL-2.0
//! `AuthStore` over PostgreSQL. The statements are the SQLite adapter's `auth.rs`, translated;
//! reads go through `reader()`, writes through `conn()`, multi-statement writes through
//! `write_tx()` — all three answer with the bound transaction's connection when there is one.
//!
//! Row mapping is by column name, as in the SQLite adapter: `SELECT *`/`RETURNING *` follow the
//! DDL's column order, which a migration may change.

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
use sqlx::{PgConnection, Row, postgres::PgRow};

use crate::{
	PgStore,
	util::{DbExt, RowExt, RowsExt, unique_as_conflict},
};

// ---------------------------------------------------------------- row mapping

/// An operator is an `ADMIN`-or-`OWNER` on the root org. Every `SELECT` feeding [`account_row`]
/// aliases this in, so the flag still travels with the row.
const IS_ROOT_ADMIN: &str = "EXISTS (SELECT 1 FROM memberships m \
	   WHERE m.account_id = accounts.id \
	     AND m.org_id = (SELECT id FROM orgs WHERE kind = 'ROOT') \
	     AND m.role IN ('ADMIN', 'OWNER') AND m.accepted_at IS NOT NULL) AS is_root_admin";

fn account_row(row: &PgRow) -> ClResult<Account> {
	Ok(Account {
		id: row.try_get("id").db()?,
		uid: AccountId::from_trusted(row.try_get::<String, _>("uid").db()?),
		email: row.try_get("email").db()?,
		pwd_hash: row.try_get("pwd_hash").db()?,
		name: row.try_get("name").db()?,
		locale: row.try_get("locale").db()?,
		status: row.try_get::<String, _>("status").db()?.parse()?,
		token_epoch: row.try_get("token_epoch").db()?,
		is_root_admin: row.try_get("is_root_admin").db()?,
		failed_logins: row.try_get("failed_logins").db()?,
		locked_until: row.try_get::<Option<i64>, _>("locked_until").db()?.map(Timestamp),
		activated_at: row.try_get::<Option<i64>, _>("activated_at").db()?.map(Timestamp),
		last_login_at: row.try_get::<Option<i64>, _>("last_login_at").db()?.map(Timestamp),
		anonymized_at: row.try_get::<Option<i64>, _>("anonymized_at").db()?.map(Timestamp),
		created_at: Timestamp(row.try_get("created_at").db()?),
	})
}

fn org_row(row: &PgRow) -> ClResult<Org> {
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

fn account_org_row(row: &PgRow) -> ClResult<AccountOrg> {
	Ok(AccountOrg {
		uid: OrgId::from_trusted(row.try_get::<String, _>("uid").db()?),
		kind: row.try_get::<String, _>("kind").db()?.parse()?,
		name: row.try_get("name").db()?,
		status: row.try_get::<String, _>("status").db()?.parse()?,
		role: row.try_get::<String, _>("role").db()?.parse()?,
		accepted_at: row.try_get::<Option<i64>, _>("accepted_at").db()?.map(Timestamp),
	})
}

fn member_row(row: &PgRow) -> ClResult<Member> {
	Ok(Member {
		account_uid: AccountId::from_trusted(row.try_get::<String, _>("account_uid").db()?),
		email: row.try_get("email").db()?,
		name: row.try_get("name").db()?,
		role: row.try_get::<String, _>("role").db()?.parse()?,
		status: row.try_get::<String, _>("status").db()?.parse()?,
		created_at: Timestamp(row.try_get("created_at").db()?),
	})
}

fn webauthn_row(row: &PgRow) -> ClResult<WebauthnCredential> {
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

fn totp_row(row: &PgRow) -> ClResult<TotpCredential> {
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

fn legal_doc_row(row: &PgRow) -> ClResult<LegalDoc> {
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

fn consent_row(row: &PgRow) -> ClResult<Consent> {
	Ok(Consent {
		id: row.try_get("id").db()?,
		kind: row.try_get::<String, _>("kind").db()?.parse()?,
		org_uid: row.try_get::<Option<String>, _>("org_uid").db()?.map(OrgId::from_trusted),
		doc_version: row.try_get("doc_version").db()?,
		doc_sha256: row.try_get("doc_sha256").db()?,
		// `BIGINT` 0/1, as SQLite stored it.
		granted: row.try_get::<i64, _>("granted").db()? != 0,
		at: Timestamp(row.try_get("at").db()?),
		withdrawn_at: row.try_get::<Option<i64>, _>("withdrawn_at").db()?.map(Timestamp),
	})
}

/// [`dump`]'s alias for the row being exported. Not a table name in this schema, so a
/// correlated subquery over the *same* table cannot shadow it.
const SRC: &str = "\"_src\"";

/// Every row of `table` matching `where_sql` as a JSON array, restricted to `columns` — the
/// closed allowlist `mintworks-auth` hands down (`mintworks_auth::gdpr::EXPORT`). Rendering rules
/// are the SQLite adapter's `dump`: an integer `*_id` with a foreign key to a table with a `uid`
/// becomes `…Uid` (else dropped), keys are camelCase, `at`/`*_at` render ISO-8601 UTC, a masked
/// column keeps its key and renders NULL where its predicate excludes the row, and a column the
/// table lacks is an error.
///
/// `where_sql` takes exactly one `$1`, bound to `id`. Table, column and mask names come from a
/// `&'static` allowlist and from the catalog, never from a request.
async fn dump(
	conn: &mut PgConnection,
	table: &str,
	where_sql: &str,
	id: i64,
	columns: &[&str],
	mask: &[(&str, &str)],
) -> ClResult<Value> {
	let types: Vec<(String, String)> = sqlx::query_as(
		"SELECT column_name::text, data_type::text FROM information_schema.columns \
		 WHERE table_schema = current_schema() AND table_name = $1",
	)
	.bind(table)
	.fetch_all(&mut *conn)
	.await
	.db()?;
	// Single-column FKs only: the framework schema declares no composite one.
	let fks: Vec<(String, String)> = sqlx::query_as(
		"SELECT a.attname::text, c.confrelid::regclass::text FROM pg_constraint c \
		 JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = c.conkey[1] \
		 WHERE c.contype = 'f' AND c.conrelid = $1::text::regclass",
	)
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
			&& ty == "bigint"
		{
			let Some((_, referenced)) = fks.iter().find(|(from, _)| from == col) else {
				continue;
			};
			if !has_column(&mut *conn, referenced, "uid").await? {
				continue;
			}
			key = camel(&format!("{stem}_uid"));
			// Qualified with the outer alias: `invoices.original_invoice_id` is self-referential.
			format!("(SELECT \"uid\" FROM \"{referenced}\" WHERE id = {SRC}.\"{col}\")")
		} else if col == "at" || col.ends_with("_at") {
			format!(
				"to_char(to_timestamp({SRC}.\"{col}\") AT TIME ZONE 'UTC', \
				 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')"
			)
		} else {
			format!("{SRC}.\"{col}\"")
		};
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
		"SELECT COALESCE(json_agg(json_build_object({}) ORDER BY {SRC}), '[]'::json)::text \
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
	conn: &mut PgConnection,
	account_id: i64,
	sections: &[ExportSection],
) -> ClResult<Vec<Value>> {
	let mut out = Vec::with_capacity(sections.len());
	for section in sections {
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

async fn has_column(conn: &mut PgConnection, table: &str, column: &str) -> ClResult<bool> {
	sqlx::query_scalar(
		"SELECT EXISTS (SELECT 1 FROM information_schema.columns \
		 WHERE table_schema = current_schema() AND table_name = $1 AND column_name = $2)",
	)
	.bind(table)
	.bind(column)
	.fetch_one(conn)
	.await
	.db()
}

/// The `WHERE` fragment behind each [`ExportScope`], taking exactly one `$1` bound to the
/// account id. The scopes themselves are `mintworks_auth::gdpr::EXPORT`'s.
fn where_of(scope: ExportScope) -> &'static str {
	match scope {
		ExportScope::Account => "id = $1",
		ExportScope::AccountId => "account_id = $1",
		ExportScope::MemberOrg => "id IN (SELECT org_id FROM memberships WHERE account_id = $1)",
		ExportScope::PersonalOrg => {
			"org_id IN (SELECT id FROM orgs WHERE owner_account_id = $1 AND kind = 'PERSONAL')"
		}
		ExportScope::PersonalOrgInvoice => {
			"invoice_id IN (SELECT id FROM invoices WHERE \
			 org_id IN (SELECT id FROM orgs WHERE owner_account_id = $1 AND kind = 'PERSONAL'))"
		}
	}
}

/// `"a" = $n, "b" = NULL` for an erasure allowlist, numbering from `$first`; returns the clause
/// and the next free placeholder. A `None` is a `NULL` literal, not a bound one: a NULL bound as
/// text is a type error against a `BIGINT` column (`accounts.pending_ref_id`). Bind the `Some`
/// values with [`erased_values`], in the same order.
fn set_clause(cols: &[ErasedCol], first: usize) -> (String, usize) {
	let mut n = first;
	let sql = cols
		.iter()
		.map(|(col, value)| match value {
			Some(_) => {
				n += 1;
				format!("\"{col}\" = ${}", n - 1)
			}
			None => format!("\"{col}\" = NULL"),
		})
		.collect::<Vec<_>>()
		.join(", ");
	(sql, n)
}

fn erased_values(cols: &[ErasedCol]) -> impl Iterator<Item = &'static str> + '_ {
	cols.iter().filter_map(|(_, value)| *value)
}

/// Whether `name` exists in the current schema — `mintworks-invoice`'s tables are absent in a
/// deployment that does not use it. Generic so a caller inside a write transaction asks on it.
async fn has_table<'e, E>(ex: E, name: &str) -> ClResult<bool>
where
	E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
	sqlx::query_scalar(
		"SELECT EXISTS (SELECT 1 FROM information_schema.tables \
		 WHERE table_schema = current_schema() AND table_name = $1)",
	)
	.bind(name)
	.fetch_one(ex)
	.await
	.db()
}

#[async_trait]
impl AuthStore for PgStore {
	// -- accounts

	async fn create_account(
		&self,
		new: &NewAccount,
		consents: &[NewConsent],
	) -> ClResult<(Account, Org)> {
		let now = Timestamp::now();
		let tx = self.write_tx().await?;

		let account_uid = AccountId::generate();
		let row = sqlx::query(
			"INSERT INTO accounts (uid, email, pwd_hash, name, locale, created_at)
			 VALUES ($1, $2, $3, $4, $5, $6) RETURNING *, false AS is_root_admin",
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
			 VALUES ($1, (SELECT id FROM orgs WHERE kind = 'ROOT'), 'PERSONAL', $2, $3, $4)
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

		// Accepted on creation, or `mintworks_auth::token::pick_org` skips it and login mints a
		// token with no `org` claim.
		sqlx::query(
			"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
			 VALUES ($1, $2, 'OWNER', $3, $4)",
		)
		.bind(org.id)
		.bind(account.id)
		.bind(now.0)
		.bind(now.0)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;

		// In the same transaction as the account: an account with no ToS/privacy rows is blocked
		// by `consent::gate` on every gated route, unrepairably.
		for c in consents {
			sqlx::query(
				"INSERT INTO consents
					(account_id, org_id, kind, legal_doc_id, doc_version, doc_sha256,
					 granted, at, ip, user_agent)
				 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
			)
			.bind(account.id)
			.bind(c.org_id)
			.bind(c.kind.as_str())
			.bind(c.legal_doc_id)
			.bind(&c.doc_version)
			.bind(&c.doc_sha256)
			.bind(i64::from(c.granted))
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
			"SELECT *, {IS_ROOT_ADMIN} FROM accounts WHERE email = $1"
		)))
		.bind(email)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.one(account_row)
	}

	async fn account_by_uid(&self, uid: &AccountId) -> ClResult<Option<Account>> {
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"SELECT *, {IS_ROOT_ADMIN} FROM accounts WHERE uid = $1"
		)))
		.bind(uid.as_str())
		.fetch_optional(&mut *self.reader().await?)
		.await
		.one(account_row)
	}

	async fn accounts_by_uid(&self, uids: &[AccountId]) -> ClResult<Vec<Account>> {
		let uids: Vec<&str> = uids.iter().map(AccountId::as_str).collect();
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"SELECT *, {IS_ROOT_ADMIN} FROM accounts WHERE uid = ANY($1)"
		)))
		.bind(uids)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(account_row)
	}

	async fn account_by_id(&self, id: i64) -> ClResult<Option<Account>> {
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"SELECT *, {IS_ROOT_ADMIN} FROM accounts WHERE id = $1"
		)))
		.bind(id)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.one(account_row)
	}

	async fn pending_ref(&self, id: i64) -> ClResult<Option<mintworks_core::ids::RefId>> {
		let uid: Option<String> = sqlx::query_scalar(
			"SELECT r.uid FROM accounts a JOIN refs r ON r.id = a.pending_ref_id WHERE a.id = $1",
		)
		.bind(id)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.db()?;
		Ok(uid.map(mintworks_core::ids::RefId::from_trusted))
	}

	async fn set_pending_ref(&self, id: i64, ref_id: i64) -> ClResult<bool> {
		let res = sqlx::query(
			"UPDATE accounts SET pending_ref_id = $1
			 WHERE id = $2 AND status = 'PENDING' AND pending_ref_id IS NULL",
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
			"UPDATE accounts SET status = 'ACTIVE', activated_at = $1,
				 pwd_hash = COALESCE($2, pwd_hash)
			 WHERE id = $3 AND status = 'PENDING'",
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
			"UPDATE accounts SET pwd_hash = $1, token_epoch = token_epoch + 1
			 WHERE id = $2 AND token_epoch = $3",
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
		sqlx::query("UPDATE accounts SET token_epoch = token_epoch + 1 WHERE id = $1")
			.bind(id)
			.execute(&mut *self.conn().await?)
			.await
			.db()?;
		Ok(())
	}

	/// The `WHERE` predicate is the whole guarantee that GDPR erasure is irreversible: an
	/// anonymized account must never return to `ACTIVE`.
	async fn set_account_status(&self, id: i64, status: AccountStatus) -> ClResult<()> {
		// One transaction: the epoch bump is the point of a suspension, and failing alone it would
		// leave the account SUSPENDED with every issued token still live.
		let tx = self.write_tx().await?;
		let res = sqlx::query(
			"UPDATE accounts SET status = $1
			  WHERE id = $2 AND (status <> 'ANONYMIZED' OR $1 = 'ANONYMIZED')",
		)
		.bind(status.as_str())
		.bind(id)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;
		if res.rows_affected() == 0 {
			let current: Option<String> =
				sqlx::query_scalar("SELECT status FROM accounts WHERE id = $1")
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
			sqlx::query("UPDATE accounts SET token_epoch = token_epoch + 1 WHERE id = $1")
				.bind(id)
				.execute(&mut *tx.lock().await?)
				.await
				.db()?;
		}
		tx.commit().await?;
		Ok(())
	}

	async fn record_login_failure(&self, id: i64) -> ClResult<()> {
		// Unconditional: on the unknown-address branch `id` is `login::NO_ACCOUNT`, and paying the
		// writer round trip anyway keeps that branch indistinguishable.
		sqlx::query("UPDATE accounts SET failed_logins = failed_logins + 1 WHERE id = $1")
			.bind(id)
			.execute(&mut *self.conn().await?)
			.await
			.db()?;
		Ok(())
	}

	async fn record_login_success(&self, id: i64, at: Timestamp) -> ClResult<()> {
		sqlx::query(
			"UPDATE accounts SET failed_logins = 0, locked_until = NULL, last_login_at = $1
			 WHERE id = $2",
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
		let tx = self.write_tx().await?;

		let org_uid = OrgId::generate();
		let row = sqlx::query(
			"INSERT INTO orgs (uid, parent_id, kind, name, owner_account_id, billing_currency,
			 created_at) VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING *",
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

		sqlx::query(
			"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
			 VALUES ($1, $2, 'OWNER', $3, $4)",
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
		sqlx::query("SELECT * FROM orgs WHERE uid = $1")
			.bind(uid.as_str())
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(org_row)
	}

	async fn org_by_id(&self, id: i64) -> ClResult<Option<Org>> {
		sqlx::query("SELECT * FROM orgs WHERE id = $1")
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
		// A suspended root strips every inherited role, including the operator authority that
		// could un-suspend it. Probe and update share the write lock, or a reparent lands between.
		if status == Some(OrgStatus::Suspended) {
			let tx = self.write_tx().await?;
			let is_root: bool = sqlx::query_scalar(
				"SELECT EXISTS (SELECT 1 FROM orgs WHERE id = $1 AND kind = 'ROOT')",
			)
			.bind(id)
			.fetch_one(&mut *tx.lock().await?)
			.await
			.db()?;
			if is_root {
				return Err(Error::conflict("the platform root org cannot be suspended"));
			}
			write_org(&mut *tx.lock().await?, id, name, billing_currency, status).await?;
			tx.commit().await?;
			return Ok(());
		}
		// A rename takes no write lock: every `PATCH /api/org` would serialise against every writer.
		let mut conn = self.conn().await?;
		write_org(&mut conn, id, name, billing_currency, status).await
	}

	async fn set_org_slug(&self, id: i64, slug: Option<&str>) -> ClResult<()> {
		self.recoverable(async |c| {
			sqlx::query("UPDATE orgs SET slug = $1 WHERE id = $2")
				.bind(slug)
				.bind(id)
				.execute(c)
				.await
				.map_err(|e| match &e {
					sqlx::Error::Database(db) if db.is_unique_violation() => {
						mintworks_core::refs::slug_taken()
					}
					_ => crate::util::map_db(&e),
				})
		})
		.await?;
		Ok(())
	}

	async fn transfer_org_ownership(&self, org_id: i64, from: i64, to: i64) -> ClResult<bool> {
		let tx = self.write_tx().await?;

		// Re-run inside the transaction: the service checked on the reader pool, where a
		// concurrent `remove_member` is invisible.
		let ok: Option<i32> = sqlx::query_scalar(
			"SELECT 1 FROM orgs t \
			  JOIN memberships m ON m.org_id = t.id AND m.account_id = $1 \
			 WHERE t.id = $2 AND t.owner_account_id = $3 AND m.accepted_at IS NOT NULL",
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

		// Demoted to `ADMIN`, not removed: `remove_member` is the separate decision.
		for (account_id, role) in [(from, "ADMIN"), (to, "OWNER")] {
			sqlx::query("UPDATE memberships SET role = $1 WHERE org_id = $2 AND account_id = $3")
				.bind(role)
				.bind(org_id)
				.bind(account_id)
				.execute(&mut *tx.lock().await?)
				.await
				.db()?;
		}
		sqlx::query("UPDATE orgs SET owner_account_id = $1 WHERE id = $2")
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
		// A child insert's FK takes `FOR KEY SHARE` on this row, so none lands after the checks.
		sqlx::query("SELECT 1 FROM orgs WHERE id = $1 FOR UPDATE")
			.bind(org_id)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;

		let others: i64 = sqlx::query_scalar(
			"SELECT count(*) FROM memberships m JOIN orgs t ON t.id = m.org_id \
			  WHERE m.org_id = $1 AND m.accepted_at IS NOT NULL \
				AND m.account_id IS DISTINCT FROM t.owner_account_id",
		)
		.bind(org_id)
		.fetch_one(&mut *tx.lock().await?)
		.await
		.db()?;
		if others > 0 {
			return Ok(false);
		}

		// Every non-cascading `REFERENCES orgs(id)` (retention, evidence, numbering), plus
		// `objects`/`documents`, which cascade silently. `has_table` per table: a deployment
		// without `mintworks-invoice` or `mintworks-billing` lacks theirs.
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
			let kept: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
				"SELECT EXISTS (SELECT 1 FROM {table} WHERE {column} = $1)"
			)))
			.bind(org_id)
			.fetch_one(&mut *tx.lock().await?)
			.await
			.db()?;
			if kept {
				return Ok(false);
			}
		}

		let deletable: bool = sqlx::query_scalar(
			"SELECT EXISTS (SELECT 1 FROM orgs WHERE id = $1 AND kind NOT IN ('PERSONAL','ROOT'))",
		)
		.bind(org_id)
		.fetch_one(&mut *tx.lock().await?)
		.await
		.db()?;
		if !deletable {
			return Ok(false);
		}
		// `refs.org_id` does not cascade and every invite mints one.
		for sql in [
			"UPDATE accounts SET pending_ref_id = NULL \
			  WHERE pending_ref_id IN (SELECT id FROM refs WHERE org_id = $1)",
			"DELETE FROM ref_uses WHERE ref_id IN (SELECT id FROM refs WHERE org_id = $1)",
			"DELETE FROM refs WHERE org_id = $1",
		] {
			sqlx::query(sql).bind(org_id).execute(&mut *tx.lock().await?).await.db()?;
		}

		let gone =
			sqlx::query("DELETE FROM orgs WHERE id = $1 AND kind NOT IN ('PERSONAL','ROOT')")
				.bind(org_id)
				.execute(&mut *tx.lock().await?)
				.await
				.db()?;
		// FK-less, so nothing cascades. `llm_usage` stays: it is the cost ledger.
		if gone.rows_affected() > 0 {
			sqlx::query("DELETE FROM agent_runs WHERE org_id = $1")
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
			 WHERE m.account_id = $1 ORDER BY t.created_at",
		)
		.bind(account_id)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(account_org_row)
	}

	async fn owned_shared_orgs(&self, account_id: i64) -> ClResult<Vec<Org>> {
		sqlx::query(
			"SELECT * FROM orgs WHERE owner_account_id = $1 AND kind != 'PERSONAL' ORDER BY id",
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
		sqlx::query_scalar(
			"SELECT EXISTS (SELECT 1 FROM currencies WHERE code = $1 AND enabled = 1)",
		)
		.bind(code.as_str())
		.fetch_one(&mut *self.reader().await?)
		.await
		.db()
	}

	// -- memberships

	async fn membership_role(&self, org_id: i64, account_id: i64) -> ClResult<Option<Role>> {
		sqlx::query_scalar::<_, String>(
			"SELECT role FROM memberships WHERE org_id = $1 AND account_id = $2",
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
			 WHERE org_id = $1 AND account_id = $2 AND accepted_at IS NOT NULL",
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
			"SELECT created_at FROM memberships WHERE org_id = $1 AND account_id = $2",
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
			"UPDATE memberships SET accepted_at = $1
			 WHERE org_id = $2 AND account_id = $3 AND accepted_at IS NULL",
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
		// `role <> 'OWNER'` in the statement: the service checks on the reader pool, where a
		// concurrent `transfer_org_ownership` is invisible. Guards only the update branch.
		let res = sqlx::query(
			"INSERT INTO memberships (org_id, account_id, role, created_at)
			 VALUES ($1, $2, $3, $4)
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
		// No `token_epoch` bump: the epoch is account-wide, so it would sign the account out of
		// every other org.
		let res = sqlx::query(
			"DELETE FROM memberships WHERE org_id = $1 AND account_id = $2 AND role <> 'OWNER'",
		)
		.bind(org_id)
		.bind(account_id)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(res.rows_affected() == 1)
	}

	async fn members(&self, org_id: i64, limit: i64) -> ClResult<Vec<Member>> {
		sqlx::query(
			"SELECT a.uid AS account_uid, a.email AS email, a.name AS name, m.role AS role,
					a.status AS status, m.created_at AS created_at
			 FROM memberships m JOIN accounts a ON a.id = m.account_id
			 WHERE m.org_id = $1 AND m.accepted_at IS NOT NULL ORDER BY m.created_at LIMIT $2",
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
		// Under the write lock: in READ COMMITTED the count in the `WHERE` is not atomic with the
		// insert as it is under SQLite's single writer, so two mints could both see room.
		let tx = self.write_tx().await?;
		let inserted: Option<i64> = sqlx::query_scalar(
			"INSERT INTO api_keys
				(uid, org_id, account_id, name, prefix, key_hash, scopes, expires_at,
				 created_at)
			 SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9
			  WHERE (SELECT count(*) FROM api_keys
			          WHERE org_id = $2 AND revoked_at IS NULL
			            AND (expires_at IS NULL OR expires_at > $9)) < $10
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
		.bind(max_live)
		.fetch_optional(&mut *tx.lock().await?)
		.await
		.map_err(|e| unique_as_conflict(&e, "api key prefix collision"))?;
		tx.commit().await?;
		if inserted.is_none() {
			return Ok(None);
		}
		// Read back rather than mapped: one type for every key read, and `prefix` is UNIQUE.
		let row = self
			.api_key_by_prefix(&new.prefix)
			.await?
			.ok_or_else(|| Error::internal("api key vanished after insert"))?;
		Ok(Some(row))
	}

	async fn api_keys_for_org(&self, org_id: i64) -> ClResult<Vec<ApiKey>> {
		sqlx::query(sqlx::AssertSqlSafe(format!(
			"{} WHERE k.org_id = $1 ORDER BY k.created_at",
			crate::core::api_key_select()
		)))
		.bind(org_id)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(crate::core::api_key_row)
	}

	async fn revoke_api_key(&self, org_id: i64, uid: &ApiKeyId, at: Timestamp) -> ClResult<bool> {
		let res = sqlx::query(
			"UPDATE api_keys SET revoked_at = $1
			  WHERE uid = $2 AND org_id = $3 AND revoked_at IS NULL",
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
		let res = sqlx::query("UPDATE api_keys SET name = $1 WHERE uid = $2 AND org_id = $3")
			.bind(name)
			.bind(uid.as_str())
			.bind(org_id)
			.execute(&mut *self.conn().await?)
			.await
			.db()?;
		Ok(res.rows_affected() == 1)
	}

	// -- totp

	/// `confirmed_at IS NULL` is the precondition, carried in the statement rather than read
	/// first: a `confirm_totp` landing between read and upsert would be wiped.
	async fn put_totp(&self, new: &NewTotpCredential) -> ClResult<bool> {
		let res = sqlx::query(
			"INSERT INTO totp_credentials
				(account_id, secret_nonce, secret_enc, digits, period, recovery_hashes,
				 created_at)
			 VALUES ($1, $2, $3, $4, $5, $6, $7)
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
		sqlx::query("SELECT * FROM totp_credentials WHERE account_id = $1")
			.bind(account_id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(totp_row)
	}

	async fn confirm_totp(&self, account_id: i64, at: Timestamp, hashes: &str) -> ClResult<bool> {
		let res = sqlx::query(
			"UPDATE totp_credentials SET confirmed_at = $1, recovery_hashes = $2
			 WHERE account_id = $3 AND confirmed_at IS NULL",
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
			"UPDATE totp_credentials SET last_used_step = $1
			 WHERE account_id = $2 AND (last_used_step IS NULL OR last_used_step < $1)",
		)
		.bind(step)
		.bind(account_id)
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
			"UPDATE totp_credentials SET recovery_hashes = $1
			 WHERE account_id = $2 AND recovery_hashes = $3",
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
		let res = sqlx::query("DELETE FROM totp_credentials WHERE account_id = $1")
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
		// Under the write lock, as `create_api_key`: the count-guarded insert is not atomic in
		// READ COMMITTED.
		let tx = self.write_tx().await?;
		let row = sqlx::query(
			"INSERT INTO webauthn_credentials
				(account_id, credential_id, credential, name, created_at)
			 SELECT $1, $2, $3, $4, $5
			  WHERE (SELECT count(*) FROM webauthn_credentials WHERE account_id = $1) < $6
			 RETURNING *",
		)
		.bind(new.account_id)
		.bind(&new.credential_id)
		.bind(&new.credential)
		.bind(&new.name)
		.bind(new.created_at.0)
		.bind(max)
		.fetch_optional(&mut *tx.lock().await?)
		.await
		.map_err(|e| unique_as_conflict(&e, "credential already registered"))?;
		tx.commit().await?;
		// No row is the cap, not an error.
		row.map(|r| webauthn_row(&r)).transpose()
	}

	/// The usernameless-login lookup: an assertion names only the credential it used.
	async fn webauthn_by_credential_id(
		&self,
		credential_id: &str,
	) -> ClResult<Option<WebauthnCredential>> {
		sqlx::query("SELECT * FROM webauthn_credentials WHERE credential_id = $1")
			.bind(credential_id)
			.fetch_optional(&mut *self.reader().await?)
			.await
			.one(webauthn_row)
	}

	async fn webauthn_for_account(&self, account_id: i64) -> ClResult<Vec<WebauthnCredential>> {
		sqlx::query("SELECT * FROM webauthn_credentials WHERE account_id = $1 ORDER BY created_at")
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
			"UPDATE webauthn_credentials SET name = $1 WHERE credential_id = $2 AND account_id = $3",
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
			"DELETE FROM webauthn_credentials WHERE credential_id = $1 AND account_id = $2",
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
			"UPDATE webauthn_credentials SET credential = $1, last_used_at = $2
			  WHERE credential_id = $3",
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
		self.recoverable(async |c| {
			sqlx::query_scalar(
				"INSERT INTO legal_docs
				(kind, locale, version, title, body, sha256, effective_from, created_at)
			 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
			)
			.bind(new.kind.as_str())
			.bind(&new.locale)
			.bind(&new.version)
			.bind(&new.title)
			.bind(&new.body)
			.bind(&new.sha256)
			.bind(new.effective_from.0)
			.bind(Timestamp::now().0)
			.fetch_one(c)
			.await
			.map_err(|e| unique_as_conflict(&e, "this legal document version already exists"))
		})
		.await
	}

	async fn current_legal_doc(
		&self,
		kind: LegalKind,
		locale: &str,
		now: Timestamp,
	) -> ClResult<Option<LegalDoc>> {
		let exact: Option<LegalDoc> = sqlx::query(
			"SELECT * FROM legal_docs
			 WHERE kind = $1 AND locale = $2 AND effective_from <= $3
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
		// Any published locale, so a kind published in any locale always gates: an untranslated
		// locale must not sail past the consent wall. Wrong language is the lesser fault.
		sqlx::query(
			"SELECT * FROM legal_docs
			 WHERE kind = $1 AND effective_from <= $2
			 ORDER BY effective_from DESC, id DESC LIMIT 1",
		)
		.bind(kind.as_str())
		.bind(now.0)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.one(legal_doc_row)
	}

	async fn record_consent(&self, new: &NewConsent, at: Timestamp) -> ClResult<i64> {
		sqlx::query_scalar(
			"INSERT INTO consents
				(account_id, org_id, kind, legal_doc_id, doc_version, doc_sha256, granted,
				 at, ip, user_agent)
			 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING id",
		)
		.bind(new.account_id)
		.bind(new.org_id)
		.bind(new.kind.as_str())
		.bind(new.legal_doc_id)
		.bind(&new.doc_version)
		.bind(&new.doc_sha256)
		.bind(i64::from(new.granted))
		.bind(at.0)
		.bind(&new.ip)
		.bind(&new.user_agent)
		.fetch_one(&mut *self.conn().await?)
		.await
		.db()
	}

	/// `IS NOT DISTINCT FROM`, so one bound parameter serves the account-wide scope (NULL) and an
	/// org-scoped one. Ordered by `c.id` alone, to agree with `list_consents` across a clock step.
	async fn latest_consent(
		&self,
		account_id: i64,
		kind: LegalKind,
		org_id: Option<i64>,
	) -> ClResult<Option<Consent>> {
		sqlx::query(
			"SELECT c.*, t.uid AS org_uid FROM consents c
			 LEFT JOIN orgs t ON t.id = c.org_id
			 WHERE c.account_id = $1 AND c.kind = $2 AND c.org_id IS NOT DISTINCT FROM $3
			 ORDER BY c.id DESC LIMIT 1",
		)
		.bind(account_id)
		.bind(kind.as_str())
		.bind(org_id)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.one(consent_row)
	}

	/// The newest row per `(kind, org_id)` — `DISTINCT ON` in place of SQLite's bare-column
	/// `MAX(id)`. `NULLS FIRST` keeps SQLite's order, where the account-wide row leads.
	async fn list_consents(&self, account_id: i64) -> ClResult<Vec<Consent>> {
		sqlx::query(
			"SELECT DISTINCT ON (c.kind, c.org_id) c.*, t.uid AS org_uid FROM consents c
			 LEFT JOIN orgs t ON t.id = c.org_id
			 WHERE c.account_id = $1
			 ORDER BY c.kind, c.org_id NULLS FIRST, c.id DESC",
		)
		.bind(account_id)
		.fetch_all(&mut *self.reader().await?)
		.await
		.all(consent_row)
	}

	async fn withdraw_consent(&self, id: i64, at: Timestamp) -> ClResult<bool> {
		let res = sqlx::query(
			"UPDATE consents SET withdrawn_at = $1 WHERE id = $2 AND withdrawn_at IS NULL",
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
		// One connection, one snapshot: this is evidence in a GDPR request. A bound handle is
		// already inside the caller's transaction; nothing in `dump_sections` takes it again.
		if let Some(held) = self.conn.scope()? {
			let mut conn = held.lock_conn().await?;
			return dump_sections(&mut conn, account_id, sections).await;
		}
		// `REPEATABLE READ`: under the default READ COMMITTED each statement takes its own
		// snapshot, and a write between two sections tears the document.
		let mut tx = self.read_pool().begin().await.db()?;
		sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
			.execute(&mut *tx)
			.await
			.db()?;
		dump_sections(&mut tx, account_id, sections).await
	}

	async fn anonymize_account(
		&self,
		account_id: i64,
		at: Timestamp,
		plan: &ErasurePlan,
	) -> ClResult<bool> {
		// One transaction: an erasure that half-applies is the worst possible outcome here.
		let tx = self.write_tx().await?;

		// Re-run inside the transaction: a `POST /api/orgs` after the service's reader-pool
		// pre-check would erase the owner of a live organisation.
		let owned: bool = sqlx::query_scalar(
			"SELECT EXISTS (SELECT 1 FROM orgs WHERE owner_account_id = $1 AND kind != 'PERSONAL')",
		)
		.bind(account_id)
		.fetch_one(&mut *tx.lock().await?)
		.await
		.db()?;
		if owned {
			return Ok(false);
		}

		// Read before the statement below destroys it: the only handle on the `SEND_EMAIL`
		// payloads addressed to this person.
		let email: Option<String> = sqlx::query_scalar("SELECT email FROM accounts WHERE id = $1")
			.bind(account_id)
			.fetch_optional(&mut *tx.lock().await?)
			.await
			.db()?;

		// Placeholder built from `uid`, not `id`: `members()` returns `accounts.email`, so the
		// internal key would leak through `GET /api/org/members`.
		let (mut set, n) = set_clause(plan.accounts, 1);
		if !set.is_empty() {
			set.push_str(", ");
		}
		let sql = format!(
			"UPDATE accounts \
			 SET {set}email = 'anonymized+' || uid || '@invalid', status = 'ANONYMIZED', \
				 anonymized_at = ${n}, token_epoch = token_epoch + 1 \
			 WHERE id = ${}",
			n + 1
		);
		let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
		for value in erased_values(plan.accounts) {
			q = q.bind(value);
		}
		q.bind(at.0).bind(account_id).execute(&mut *tx.lock().await?).await.db()?;

		let (set, n) = set_clause(plan.agent_runs, 1);
		if !set.is_empty() {
			// Before the UPDATE: it nulls `account_id`, the only handle on the account's runs.
			sqlx::query(
				"DELETE FROM agent_run_events
				 WHERE run_id IN (SELECT id FROM agent_runs WHERE account_id = $1)",
			)
			.bind(account_id)
			.execute(&mut *tx.lock().await?)
			.await
			.db()?;
			let sql = format!("UPDATE agent_runs SET {set} WHERE account_id = ${n}");
			let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
			for value in erased_values(plan.agent_runs) {
				q = q.bind(value);
			}
			q.bind(account_id).execute(&mut *tx.lock().await?).await.db()?;
		}

		for table in plan.delete_by_account {
			let sql = format!("DELETE FROM \"{table}\" WHERE account_id = $1");
			sqlx::query(sqlx::AssertSqlSafe(sql))
				.bind(account_id)
				.execute(&mut *tx.lock().await?)
				.await
				.db()?;
		}

		// `account_id`, not the org: a key belongs to the person.
		sqlx::query(
			"UPDATE api_keys SET revoked_at = $1 WHERE revoked_at IS NULL AND account_id = $2",
		)
		.bind(at.0)
		.bind(account_id)
		.execute(&mut *tx.lock().await?)
		.await
		.db()?;

		// `kind = 'PERSONAL'`: an organisation this account merely owns keeps its trading name.
		if !plan.orgs.is_empty() {
			let (set, n) = set_clause(plan.orgs, 1);
			let sql = format!(
				"UPDATE orgs SET {set} WHERE owner_account_id = ${n} AND kind = 'PERSONAL'"
			);
			let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
			for value in erased_values(plan.orgs) {
				q = q.bind(value);
			}
			q.bind(account_id).execute(&mut *tx.lock().await?).await.db()?;
		}

		// Natural persons (`kind = 'P'`) under the account's own personal org only: rows under an
		// organisation it merely owns are other people's data.
		if !plan.billing_parties.is_empty()
			&& has_table(&mut *tx.lock().await?, "billing_parties").await?
		{
			let (set, n) = set_clause(plan.billing_parties, 1);
			let sql = format!(
				"UPDATE billing_parties SET {set} \
				 WHERE kind = 'P' \
				   AND org_id IN ( \
						SELECT id FROM orgs WHERE owner_account_id = ${n} AND kind = 'PERSONAL' \
				   )"
			);
			let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
			for value in erased_values(plan.billing_parties) {
				q = q.bind(value);
			}
			q.bind(account_id).execute(&mut *tx.lock().await?).await.db()?;
		}

		// The personal org is the only handle on its `objects`; the row stays and the body is
		// blanked, and `object_index`, derived from `body`, goes in the same transaction.
		if !plan.objects.is_empty() {
			let personal = |n: usize| {
				format!(
					"org_id IN (SELECT id FROM orgs \
					 WHERE owner_account_id = ${n} AND kind = 'PERSONAL')"
				)
			};
			let (set, n) = set_clause(plan.objects, 1);
			let sql = format!("UPDATE objects SET {set} WHERE {}", personal(n));
			let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
			for value in erased_values(plan.objects) {
				q = q.bind(value);
			}
			q.bind(account_id).execute(&mut *tx.lock().await?).await.db()?;

			let sql = format!(
				"DELETE FROM object_index \
				  WHERE object_id IN (SELECT id FROM objects WHERE {})",
				personal(1)
			);
			sqlx::query(sqlx::AssertSqlSafe(sql))
				.bind(account_id)
				.execute(&mut *tx.lock().await?)
				.await
				.db()?;
		}

		if let Some(email) = email {
			// Not restricted to FAILED: a PENDING row for an erased account must not be delivered
			// either. Matched on the parsed `to` field, in Rust: a `::jsonb` cast in SQL aborts the
			// erasure on one malformed payload, and `IS JSON` needs PostgreSQL 16.
			if !plan.blank_job_kinds.is_empty() {
				let kinds: Vec<&str> = plan.blank_job_kinds.to_vec();
				let rows: Vec<(i64, String)> = sqlx::query_as(
					"SELECT id, payload FROM jobs WHERE kind = ANY($1) AND payload <> ''",
				)
				.bind(&kinds)
				.fetch_all(&mut *tx.lock().await?)
				.await
				.db()?;
				let ids: Vec<i64> = rows
					.into_iter()
					.filter(|(_, payload)| {
						serde_json::from_str::<Value>(payload).ok().is_some_and(|v| {
							v.get("to").and_then(Value::as_str) == Some(email.as_str())
						})
					})
					.map(|(id, _)| id)
					.collect();
				if !ids.is_empty() {
					sqlx::query("UPDATE jobs SET payload = '' WHERE id = ANY($1)")
						.bind(&ids)
						.execute(&mut *tx.lock().await?)
						.await
						.db()?;
				}
			}
			// The placeholder, not NULL: a NULL `refs.email` means anyone may use the ref.
			sqlx::query(
				"UPDATE refs SET email = (SELECT email FROM accounts WHERE id = $1) WHERE email = $2",
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
	conn: &mut PgConnection,
	id: i64,
	name: Option<&str>,
	billing_currency: Patch<CurrencyCode>,
	status: Option<OrgStatus>,
) -> ClResult<()> {
	let currency = billing_currency.as_option();
	sqlx::query(
		"UPDATE orgs SET
			name = COALESCE($1, name),
			billing_currency = CASE WHEN $2 THEN $3 ELSE billing_currency END,
			status = COALESCE($4, status)
		 WHERE id = $5",
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
