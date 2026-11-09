// SPDX-License-Identifier: MPL-2.0
//! `invoice_emails`: the [`InvoiceStore`](mintworks_invoice::store::InvoiceStore) mail methods.

use mintworks_core::ids::MailId;
use mintworks_core::prelude::*;
use mintworks_invoice::mail::{KIND_MAIL, MailCounts};
use mintworks_invoice::store::{
	InvoiceMail, MailCaps, MailProblem, MailQueued, MailReason, MailStatus, NewMail,
};
use sqlx::{Row, sqlite::SqliteRow};

use crate::SqliteStore;
use crate::core::JOB_INSERT;
use crate::util::{DbExt, RowExt, RowsExt};

fn mail_row(row: &SqliteRow) -> ClResult<InvoiceMail> {
	Ok(InvoiceMail {
		id: row.try_get("id").db()?,
		uid: MailId::from_trusted(row.try_get("uid").db()?),
		org_id: row.try_get("org_id").db()?,
		invoice_id: row.try_get("invoice_id").db()?,
		kind: row.try_get::<String, _>("kind").db()?.parse()?,
		status: row.try_get::<String, _>("status").db()?.parse()?,
		recipient: row.try_get("recipient").db()?,
		cc: serde_json::from_str(&row.try_get::<String, _>("cc").db()?)
			.map_err(|e| Error::internal(format!("invoice_emails.cc: {e}")))?,
		reason: row
			.try_get::<Option<String>, _>("reason")
			.db()?
			.map(|r| r.parse())
			.transpose()?,
		at: Timestamp(row.try_get("at").db()?),
		manual: row.try_get::<i64, _>("manual").db()? != 0,
	})
}

/// A row that sent nothing: it neither counts toward a cap nor starts the resend gap. The daily
/// cap is the seller's, per mode: a buyer org cannot spend it, nor test mail the live one.
const COUNTED: &str = "e.status <> 'NO_EMAIL' AND COALESCE(e.reason, '') <> 'DAILY_LIMIT'";

pub(crate) async fn mail_queue(
	s: &SqliteStore,
	m: &NewMail,
	caps: &MailCaps,
) -> ClResult<MailQueued> {
	let tx = s.write_tx().await?;
	let mut c = tx.lock().await?;
	let exists: Option<i64> =
		sqlx::query_scalar("SELECT 1 FROM invoice_emails WHERE invoice_id = ? LIMIT 1")
			.bind(m.invoice_id)
			.fetch_optional(&mut *c)
			.await
			.db()?;
	let mut counts = MailCounts { exists: exists.is_some(), ..MailCounts::default() };
	if caps.resend.is_some() {
		(counts.resend_n, counts.resend_last) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
			"SELECT COUNT(*), MAX(e.at) FROM invoice_emails e WHERE e.invoice_id = ? AND {COUNTED}"
		)))
		.bind(m.invoice_id)
		.fetch_one(&mut *c)
		.await
		.db()?;
	}
	counts.day_recipients = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
		"SELECT COALESCE(SUM(1 + json_array_length(e.cc)), 0) FROM invoice_emails e
		 JOIN invoices i ON i.id = e.invoice_id
		 JOIN invoices me ON me.id = ? AND me.seller_id = i.seller_id AND me.test_mode = i.test_mode
		 WHERE e.at >= ? AND {COUNTED}"
	)))
	.bind(m.invoice_id)
	.bind(caps.day_start.0)
	.fetch_one(&mut *c)
	.await
	.db()?;
	let (status, reason) = match mintworks_invoice::mail::decide(m, caps, &counts) {
		Ok(d) => d,
		Err(refused) => return Ok(refused),
	};
	let cc = serde_json::to_string(&m.cc)
		.map_err(|e| Error::internal(format!("invoice_emails.cc: {e}")))?;
	let id: i64 = sqlx::query_scalar(
		"INSERT INTO invoice_emails
		 (uid, org_id, invoice_id, kind, status, recipient, cc, reason, at, manual)
		 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
	)
	.bind(m.uid.as_str())
	.bind(m.org_id)
	.bind(m.invoice_id)
	.bind(m.kind.as_str())
	.bind(status.as_str())
	.bind(&m.recipient)
	.bind(&cc)
	.bind(reason.map(MailReason::as_str))
	.bind(m.at.0)
	.bind(i64::from(m.manual))
	.fetch_one(&mut *c)
	.await
	.db()?;
	if status == MailStatus::Queued {
		let payload = serde_json::json!({
			"orgId": m.org_id, "invoiceId": m.invoice_id, "mailUid": m.uid.as_str(),
		})
		.to_string();
		let _: Option<i64> = sqlx::query_scalar(JOB_INSERT)
			.bind(KIND_MAIL)
			.bind(payload)
			.bind(format!("invmail:{}", m.uid.as_str()))
			.bind(m.at.0)
			.bind(m.at.0)
			.fetch_optional(&mut *c)
			.await
			.db()?;
	}
	drop(c);
	tx.commit().await?;
	Ok(MailQueued::Written(InvoiceMail {
		id,
		uid: m.uid.clone(),
		org_id: m.org_id,
		invoice_id: m.invoice_id,
		kind: m.kind,
		status,
		recipient: m.recipient.clone(),
		cc: m.cc.clone(),
		reason,
		at: m.at,
		manual: m.manual,
	}))
}

pub(crate) async fn mail_rows(
	s: &SqliteStore,
	org_id: i64,
	invoice_id: i64,
) -> ClResult<Vec<InvoiceMail>> {
	sqlx::query("SELECT * FROM invoice_emails WHERE org_id = ? AND invoice_id = ? ORDER BY id DESC")
		.bind(org_id)
		.bind(invoice_id)
		.fetch_all(&mut *s.reader().await?)
		.await
		.all(mail_row)
}

pub(crate) async fn mail_by_uid(s: &SqliteStore, uid: &str) -> ClResult<Option<InvoiceMail>> {
	sqlx::query("SELECT * FROM invoice_emails WHERE uid = ?")
		.bind(uid)
		.fetch_optional(&mut *s.reader().await?)
		.await
		.one(mail_row)
}

pub(crate) async fn mail_mark(
	s: &SqliteStore,
	uid: &str,
	status: MailStatus,
	reason: Option<MailReason>,
) -> ClResult<bool> {
	let done = sqlx::query(
		"UPDATE invoice_emails SET status = ?, reason = ? WHERE uid = ? AND status = 'QUEUED'",
	)
	.bind(status.as_str())
	.bind(reason.map(MailReason::as_str))
	.bind(uid)
	.execute(&mut *s.conn().await?)
	.await
	.db()?;
	Ok(done.rows_affected() > 0)
}

pub(crate) async fn mail_pending(s: &SqliteStore, limit: i64) -> ClResult<Vec<InvoiceMail>> {
	sqlx::query("SELECT * FROM invoice_emails WHERE status = 'QUEUED' ORDER BY at, id LIMIT ?")
		.bind(limit)
		.fetch_all(&mut *s.reader().await?)
		.await
		.all(mail_row)
}

pub(crate) async fn set_mail_optout(s: &SqliteStore, invoice_id: i64, on: bool) -> ClResult<()> {
	sqlx::query("UPDATE invoices SET mail_optout = ? WHERE id = ?")
		.bind(i64::from(on))
		.bind(invoice_id)
		.execute(&mut *s.conn().await?)
		.await
		.db()?;
	Ok(())
}

pub(crate) async fn mail_optout(s: &SqliteStore, invoice_id: i64) -> ClResult<bool> {
	let on: Option<i64> = sqlx::query_scalar("SELECT mail_optout FROM invoices WHERE id = ?")
		.bind(invoice_id)
		.fetch_optional(&mut *s.reader().await?)
		.await
		.db()?;
	Ok(on.unwrap_or(0) != 0)
}

fn problem_row(row: &SqliteRow) -> ClResult<(i64, MailProblem)> {
	let status = row.try_get::<String, _>("status").db()?.parse()?;
	let at = Timestamp(row.try_get("at").db()?);
	Ok((row.try_get("invoice_id").db()?, MailProblem { status, at }))
}

pub(crate) async fn mail_problems(
	s: &SqliteStore,
	org_id: i64,
	ids: &[i64],
	since: Timestamp,
) -> ClResult<Vec<(i64, MailProblem)>> {
	// Both inlined values are integers; `in_ids` owns the binds.
	let head = format!(
		"SELECT e.invoice_id, e.status, e.at FROM invoice_emails e
		 WHERE e.org_id = {org_id} AND e.status IN ('NO_EMAIL','FAILED') AND e.at >= {}
		 AND NOT EXISTS (SELECT 1 FROM invoice_emails s WHERE s.invoice_id = e.invoice_id
		   AND s.status = 'SENT' AND s.id > e.id)
		 AND e.invoice_id IN",
		since.0
	);
	s.in_ids(&head, " ORDER BY e.id DESC", ids, problem_row).await
}

// vim: ts=4
