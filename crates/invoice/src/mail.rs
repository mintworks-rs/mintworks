// SPDX-License-Identifier: MPL-2.0
//! Invoice email: an issued invoice or storno goes to its billing party as a signed download
//! link plus the PDF's SHA-256, never as an attachment, so every fetch is audited.
//!
//! [`crate::service_api::Invoices::mail_auto`] / `mail_resend` write the `invoice_emails` row and
//! its `INVOICE_MAIL` job (`invmail:{uid}`) in one transaction ([`InvoiceStore::mail_queue`]).
//! The job waits for the PDF, then hands a `SEND_EMAIL` job keyed `invmail-send:{uid}` over,
//! still `QUEUED`; `INVOICE_MAIL_SETTLE` moves the row to `SENT`/`FAILED` by that job's real
//! outcome, and never fails a row whose own `invmail:` job is still live. The keyed hand-off is
//! the at-most-once guarantee: `job_sweep` never deletes a keyed row.

use std::sync::Arc;

use mintworks_core::app::App;
use mintworks_core::ctx::Ctx;
use mintworks_core::job::{self, Job, Next, Runner};
use mintworks_core::prelude::*;
use mintworks_core::settings::SettingValue;
use serde::{Deserialize, Serialize};

use crate::store::{
	BillingParty, InvoiceKind, InvoiceMail, InvoiceStore, MailCaps, MailKind, MailQueued,
	MailReason, MailStatus, NewMail,
};

pub const KIND_MAIL: &str = "INVOICE_MAIL";
pub const KIND_SETTLE: &str = "INVOICE_MAIL_SETTLE";
/// PDF waiting tops out at `PDF_TRIES` × `PDF_RETRY_SECS`, ten minutes.
const PDF_TRIES: i64 = 20;
const PDF_RETRY_SECS: i64 = 30;
/// A `QUEUED` row this old with no send job and no live `invmail:` job lost its job.
const STALE_SECS: i64 = 3600;
const SETTLE_BATCH: i64 = 500;
/// `mintworks_email::job::MAX_CC`; this crate does not depend on `mintworks-email`.
const MAX_CC: usize = 10;
const INTRO_MAX: usize = 1000;

/// One `invoice_emails` row on the wire.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MailView {
	pub uid: String,
	pub kind: MailKind,
	pub status: MailStatus,
	pub recipient: String,
	pub cc: Vec<String>,
	pub reason: Option<MailReason>,
	pub at: Timestamp,
	pub manual: bool,
}

impl MailView {
	#[must_use]
	pub fn of(r: &InvoiceMail) -> Self {
		Self {
			uid: r.uid.as_str().to_owned(),
			kind: r.kind,
			status: r.status,
			recipient: r.recipient.clone(),
			cc: r.cc.clone(),
			reason: r.reason,
			at: r.at,
			manual: r.manual,
		}
	}
}

/// The answer to a manual send: the new row, or `NoEmail` with no row (`uid` empty).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MailSent {
	pub uid: String,
	pub status: MailStatus,
	pub recipient: String,
}

/// The `To` and `Cc` of a party's mail.
#[derive(Debug, PartialEq, Eq)]
pub struct Recipients {
	/// The first valid address, `""` when none.
	pub to: String,
	pub cc: Vec<String>,
}

/// A bare address `SEND_EMAIL` accepts: one `@`, a dotted domain, and nothing that would split
/// a header or a list.
#[must_use]
pub fn valid_email(s: &str) -> bool {
	mintworks_core::settings::check_address(s).is_ok()
		&& !s.is_empty()
		&& !s.chars().any(|c| c.is_whitespace() || matches!(c, ',' | ';' | '<' | '>' | '"'))
}

/// `emails[]` (else `email`, also when no `emails[]` entry is valid): the first valid address
/// is `to`, the other valid ones `cc`, deduplicated case-insensitively and capped at [`MAX_CC`].
#[must_use]
pub fn recipients(party: &BillingParty) -> Recipients {
	let valid = |all: &[String]| {
		let mut seen: Vec<String> = Vec::new();
		for e in all.iter().map(|e| e.trim()) {
			if valid_email(e) && !seen.iter().any(|s| s.eq_ignore_ascii_case(e)) {
				seen.push(e.to_owned());
			}
		}
		seen
	};
	let mut seen = valid(&party.emails);
	if seen.is_empty() {
		seen = valid(party.email.as_slice());
	}
	let mut it = seen.into_iter();
	let to = it.next().unwrap_or_default();
	Recipients { to, cc: it.take(MAX_CC).collect() }
}

/// `2026-11-30` as `2026. 11. 30.` (`hu`, `hu-en`) or `30 Nov 2026` (`en`); `None` is `""` and
/// anything not shaped like a date passes through.
#[must_use]
pub fn fmt_date(iso: Option<&str>, lang: &str) -> String {
	const MONTHS: [&str; 12] = [
		"Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
	];
	let raw = iso.unwrap_or_default();
	let parts: Vec<&str> = raw.split('-').collect();
	let [year, month, day] = parts[..] else { return raw.to_owned() };
	if year.len() != 4 || month.len() != 2 || day.len() != 2 {
		return raw.to_owned();
	}
	if lang != "en" {
		return format!("{year}. {month}. {day}.");
	}
	let (Ok(mi), Ok(di)) = (month.parse::<usize>(), day.parse::<u32>()) else {
		return raw.to_owned();
	};
	match MONTHS.get(mi.wrapping_sub(1)) {
		Some(name) => format!("{di} {name} {year}"),
		None => raw.to_owned(),
	}
}

/// A wire amount (`-158750.00`) as `-158 750 Ft` (`hu`, `hu-en`) or `-158,750 HUF` (`en`); zero
/// minor units are dropped.
#[must_use]
pub fn fmt_amount(amount: &str, currency: &str, lang: &str) -> String {
	let en = lang == "en";
	let (sign, rest) = amount.strip_prefix('-').map_or(("", amount), |r| ("-", r));
	let (int, frac) = rest.split_once('.').unwrap_or((rest, ""));
	let mut out = String::new();
	for (i, c) in int.chars().enumerate() {
		if i > 0 && (int.len() - i) % 3 == 0 {
			out.push(if en { ',' } else { ' ' });
		}
		out.push(c);
	}
	if !frac.is_empty() && frac.chars().any(|c| c != '0') {
		out.push(if en { '.' } else { ',' });
		out.push_str(frac);
	}
	let cur = if currency == "HUF" && !en { "Ft" } else { currency };
	format!("{sign}{out} {cur}")
}

/// The org's intro text for `kind` in `lang`, from `invoice.mail.intro`; `hu-en` is the
/// Hungarian, a blank line, then the English.
#[must_use]
pub fn intro(all: &serde_json::Value, kind: MailKind, lang: &str) -> String {
	let t = &all[if kind == MailKind::Storno { "storno" } else { "invoice" }];
	let hu = t["hu"].as_str().unwrap_or_default().trim();
	let en = t["en"].as_str().unwrap_or_default().trim();
	match lang {
		"hu" => hu.to_owned(),
		"en" => en.to_owned(),
		_ if hu.is_empty() => en.to_owned(),
		_ if en.is_empty() => hu.to_owned(),
		_ => format!("{hu}\n\n{en}"),
	}
}

/// The subject prefix of a test-mode invoice's mail.
#[must_use]
pub fn subject_prefix(test_mode: bool, lang: &str) -> &'static str {
	match (test_mode, lang) {
		(false, _) => "",
		(true, "en") => "[TEST] ",
		(true, "hu-en") => "[TESZT/TEST] ",
		(true, _) => "[TESZT] ",
	}
}

/// `invoice.mail.intro`: `{invoice:{hu,en}, storno:{hu,en}}`, every text a string of at most
/// 1000 characters with no link. Unknown keys are refused rather than stored.
///
/// # Errors
/// `E-CORE-VALIDATION` naming the offending text.
pub fn check_intro(v: &serde_json::Value) -> ClResult<()> {
	let bad = |m: String| Err(Error::validation(m));
	let Some(o) = v.as_object() else { return bad("the intro must be an object".into()) };
	if o.keys().any(|k| k != "invoice" && k != "storno") {
		return bad("the intro takes only invoice and storno".into());
	}
	for kind in ["invoice", "storno"] {
		let Some(t) = o.get(kind).and_then(|t| t.as_object()) else {
			return bad(format!("{kind} must be an object of hu and en"));
		};
		if t.keys().any(|k| k != "hu" && k != "en") {
			return bad(format!("{kind} takes only hu and en"));
		}
		for lang in ["hu", "en"] {
			let Some(s) = t.get(lang).and_then(|s| s.as_str()) else {
				return bad(format!("{kind}.{lang} must be a string"));
			};
			if s.chars().count() > INTRO_MAX {
				return bad(format!("{kind}.{lang} must be at most {INTRO_MAX} characters"));
			}
			// `//` covers every scheme and `//host`; bare domains still pass.
			let low = s.to_lowercase();
			if ["//", "www.", "mailto:"].iter().any(|p| low.contains(p)) {
				return bad(format!("{kind}.{lang} must not contain links"));
			}
		}
	}
	Ok(())
}

/// [`check_intro`] as the `invoice.mail.intro` setting's `check`, so every write path runs it.
///
/// # Errors
/// `E-CORE-VALIDATION` when it is not JSON or [`check_intro`] refuses it.
pub fn check_intro_raw(raw: &str) -> ClResult<()> {
	let v: serde_json::Value =
		serde_json::from_str(raw).map_err(|e| Error::validation(format!("intro: {e}")))?;
	check_intro(&v)
}

/// What [`InvoiceStore::mail_queue`] counts inside its transaction for [`decide`].
#[derive(Clone, Debug, Default)]
pub struct MailCounts {
	/// The invoice has any row.
	pub exists: bool,
	pub resend_n: i64,
	pub resend_last: Option<i64>,
	/// The seller's recipients (`To` plus `Cc`) in this invoice's mode since `day_start`.
	pub day_recipients: i64,
}

/// The cap policy every adapter's `mail_queue` applies: the status and reason to write, or the
/// refusal that writes nothing. Exists (automatic only) → resend caps → no address → daily cap.
///
/// # Errors
/// The [`MailQueued`] refusal (never `Written`).
pub fn decide(
	m: &NewMail,
	caps: &MailCaps,
	c: &MailCounts,
) -> Result<(MailStatus, Option<MailReason>), MailQueued> {
	if !m.manual && c.exists {
		return Err(MailQueued::Exists);
	}
	if let Some(r) = &caps.resend
		&& (c.resend_n >= r.max || c.resend_last.is_some_and(|at| at > r.gap_start.0))
	{
		return Err(MailQueued::ResendLimit);
	}
	if m.recipient.is_empty() {
		return Ok((MailStatus::NoEmail, None));
	}
	let wanted = 1 + i64::try_from(m.cc.len()).unwrap_or(i64::MAX);
	if c.day_recipients.saturating_add(wanted) > caps.daily_max {
		if m.manual {
			return Err(MailQueued::DailyLimit);
		}
		return Ok((MailStatus::Failed, Some(MailReason::DailyLimit)));
	}
	Ok((MailStatus::Queued, None))
}

/// An org-scoped integer setting.
pub(crate) async fn org_int(app: &App, org_id: i64, key: &str) -> ClResult<i64> {
	match app.settings.get_for(org_id, 0, key).await? {
		SettingValue::Int(n) => Ok(n),
		_ => Err(Error::internal(format!("{key} is not an integer setting"))),
	}
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Payload {
	mail_uid: String,
}

/// `INVOICE_MAIL` and the `INVOICE_MAIL_SETTLE` sweep. Seed the sweep with [`seed`].
pub fn register(runner: &mut Runner, app: App, store: Arc<dyn InvoiceStore>) {
	let (a, s) = (app.clone(), Arc::clone(&store));
	runner.register_next(KIND_MAIL, move |job: Job| {
		let (app, store) = (a.clone(), Arc::clone(&s));
		async move { deliver(&app, store.as_ref(), &job).await }
	});
	runner.register_periodic(KIND_SETTLE, 300, move |_job| {
		let (app, store) = (app.clone(), Arc::clone(&store));
		async move { settle(&app, store.as_ref()).await }
	});
}

/// # Errors
/// Whatever the store raises.
pub async fn seed(store: &Arc<dyn mintworks_core::store::CoreStore>) -> ClResult<()> {
	job::seed_periodic(store, KIND_SETTLE).await
}

/// The `INVOICE_MAIL` body. Public so a test can drive it without the runner.
///
/// # Errors
/// Whatever the store, the link mint or the enqueue raises; the runner retries it.
pub async fn deliver(app: &App, store: &dyn InvoiceStore, job: &Job) -> ClResult<Next> {
	let p: Payload = serde_json::from_str(&job.payload)
		.map_err(|e| Error::internal(format!("bad {KIND_MAIL} payload: {e}")))?;
	let Some(row) = store.mail_by_uid(&p.mail_uid).await? else { return Ok(Next::Done) };
	if row.status != MailStatus::Queued {
		return Ok(Next::Done);
	}
	let inv = store.invoice_by_id(row.invoice_id).await?.ok_or(Error::NotFound)?;
	if store.invoice_document(inv.id).await?.is_none() {
		if job.attempts >= PDF_TRIES {
			store
				.mail_mark(row.uid.as_str(), MailStatus::Failed, Some(MailReason::PdfTimeout))
				.await?;
			return Ok(Next::Done);
		}
		return Ok(Next::Again { at: Timestamp(Timestamp::now().0 + PDF_RETRY_SECS) });
	}
	// A retry after the hand-off must not mint another `refs` row.
	let key = format!("invmail-send:{}", row.uid.as_str());
	if !app.store.job_statuses_by_keys(std::slice::from_ref(&key)).await?.is_empty() {
		return Ok(Next::Done);
	}
	let seller = store.seller_by_id(inv.seller_id).await?.ok_or(Error::NotFound)?;
	// The version frozen on the invoice, as the PDF prints it, not the seller's current one.
	let seller_ver = inv.seller_ver.ok_or_else(|| {
		Error::internal("mintworks-invoice/mail: an issued invoice has no seller_ver")
	})?;
	let ver = store.seller_version(seller_ver).await?.ok_or(Error::NotFound)?;
	let lang = crate::draft::doc_lang(inv.doc_lang.as_deref(), inv.buyer_country.as_deref());
	let intro_all = match app.settings.get_for(seller.org_id, 0, "invoice.mail.intro").await? {
		SettingValue::Json(v) => v,
		_ => serde_json::Value::Null,
	};
	// Minted last before the enqueue: each link is a `refs` row the org admin can revoke.
	let ctx = Ctx::system("invoice.mail").with_org(row.org_id);
	let (_, doc, url, link) = crate::service_api::Invoices::new(app.clone())
		.mint_link(&ctx, inv.uid.as_str())
		.await?;
	let wire = inv.gross.to_wire(&inv.currency);
	let mut vars = serde_json::json!({
		"tenantName": ver.name,
		"number": inv.number,
		"amount": fmt_amount(&wire.amount, inv.currency.as_str(), lang),
		"dueDate": fmt_date(inv.due_date.as_deref(), lang),
		"periodStart": fmt_date(inv.period_start.as_deref(), lang),
		"periodEnd": fmt_date(inv.period_end.as_deref(), lang),
		"intro": intro(&intro_all, row.kind, lang),
		"subjectPrefix": subject_prefix(inv.test_mode, lang),
		"downloadUrl": url,
		"sha256": doc.sha256,
	});
	if inv.kind == InvoiceKind::Storno
		&& let Some(orig) = inv.original_invoice_id
	{
		let o = store.invoice_by_id(orig).await?;
		vars["originalNumber"] = serde_json::json!(o.and_then(|o| o.number));
	}
	let reply_to = ver.email.as_deref().map(str::trim).filter(|e| valid_email(e));
	let template = if row.kind == MailKind::Storno { "storno_mail" } else { "invoice_mail" };
	// `mintworks_email::SendEmail`, built by hand as `mintworks_billing::dunning` does.
	let payload = serde_json::json!({
		"to": row.recipient,
		"template": template,
		"lang": lang,
		"vars": vars,
		"cc": row.cc,
		"reply_to": reply_to,
		"from_name": ver.name,
	})
	.to_string();
	let sent =
		job::enqueue(&app.store, job::KIND_SEND_EMAIL, &payload, Some(&key), Timestamp::now())
			.await;
	if let Err(e) = sent {
		// The retry mints a new link; this one was never sent, so it must not stay live.
		let revoked = match mintworks_core::refs::Refs::from_app(app) {
			Ok(refs) => refs.revoke(&ctx, link.as_str()).await,
			Err(r) => Err(r),
		};
		if let Err(r) = revoked {
			tracing::warn!(link = %link.as_str(), error = %r, "unsent invoice link not revoked");
		}
		return Err(e);
	}
	Ok(Next::Done)
}

/// A `SEND_EMAIL` job status as the row's outcome; `None` while it may still run.
fn settled(job_status: &str) -> Option<(MailStatus, Option<MailReason>)> {
	match job_status {
		"DONE" => Some((MailStatus::Sent, None)),
		"PENDING" | "RUNNING" => None,
		_ => Some((MailStatus::Failed, Some(MailReason::SendFailed))),
	}
}

/// One `INVOICE_MAIL_SETTLE` pass: each `QUEUED` row whose `SEND_EMAIL` finished takes its
/// outcome, and one with no send job whose own `invmail:` job is gone or finished fails after
/// [`STALE_SECS`], so it can be resent.
///
/// # Errors
/// Whatever the store raises.
pub async fn settle(app: &App, store: &dyn InvoiceStore) -> ClResult<()> {
	let rows = store.mail_pending(SETTLE_BATCH).await?;
	let keys: Vec<String> = rows
		.iter()
		.flat_map(|r| {
			[
				format!("invmail-send:{}", r.uid.as_str()),
				format!("invmail:{}", r.uid.as_str()),
			]
		})
		.collect();
	let statuses: std::collections::HashMap<String, String> =
		app.store.job_statuses_by_keys(&keys).await?.into_iter().collect();
	let stale = Timestamp::now().0 - STALE_SECS;
	for (row, key) in rows.iter().zip(keys.chunks(2)) {
		// `deliver` runs as the `invmail:` job, so a live one may still hand over.
		let own_live = statuses.get(&key[1]).is_some_and(|st| settled(st).is_none());
		let outcome = match statuses.get(&key[0]) {
			Some(st) => settled(st),
			None if !own_live && row.at.0 < stale => {
				Some((MailStatus::Failed, Some(MailReason::SendFailed)))
			}
			None => None,
		};
		if let Some((status, reason)) = outcome {
			store.mail_mark(row.uid.as_str(), status, reason).await?;
		}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn party(email: Option<&str>, emails: &[&str]) -> BillingParty {
		BillingParty {
			id: 1,
			uid: mintworks_core::ids::PartyId::generate(),
			org_id: 1,
			kind: crate::store::PartyKind::Company,
			name: "Vevo".into(),
			country: "HU".into(),
			tax_number: None,
			eu_vat_id: None,
			group_tax_no: None,
			postcode: None,
			city: None,
			street: None,
			email: email.map(str::to_owned),
			emails: emails.iter().map(|s| (*s).to_owned()).collect(),
			currency: None,
			is_default: false,
			payment_days: None,
			payment_method: None,
			doc_lang: None,
			created_at: Timestamp(0),
			updated_at: Timestamp(0),
		}
	}

	#[test]
	fn recipients_take_the_first_valid_address_and_cc_the_rest_once() {
		let p = party(None, &["bad", " A@x.hu ", "b@y.hu", "a@X.HU", "c d@z.hu"]);
		assert_eq!(recipients(&p), Recipients { to: "A@x.hu".into(), cc: vec!["b@y.hu".into()] });
		let p = party(Some("only@x.hu"), &[]);
		assert_eq!(recipients(&p).to, "only@x.hu");
		assert_eq!(recipients(&party(None, &["nope"])).to, "");
		assert_eq!(recipients(&party(Some("fallback@x.hu"), &["nope"])).to, "fallback@x.hu");
		let many: Vec<String> = (0..15).map(|i| format!("u{i}@x.hu")).collect();
		let refs: Vec<&str> = many.iter().map(String::as_str).collect();
		assert_eq!(recipients(&party(None, &refs)).cc.len(), MAX_CC);
	}

	#[test]
	fn dates_follow_the_language() {
		assert_eq!(fmt_date(Some("2026-11-30"), "hu"), "2026. 11. 30.");
		assert_eq!(fmt_date(Some("2026-11-30"), "hu-en"), "2026. 11. 30.");
		assert_eq!(fmt_date(Some("2026-11-03"), "en"), "3 Nov 2026");
		assert_eq!(fmt_date(None, "en"), "");
		assert_eq!(fmt_date(Some("soon"), "hu"), "soon");
	}

	#[test]
	fn amounts_group_and_name_the_currency() {
		assert_eq!(fmt_amount("-158750.00", "HUF", "hu"), "-158 750 Ft");
		assert_eq!(fmt_amount("-158750.00", "HUF", "en"), "-158,750 HUF");
		assert_eq!(fmt_amount("1234.50", "EUR", "hu-en"), "1 234,50 EUR");
		assert_eq!(fmt_amount("999", "EUR", "en"), "999 EUR");
	}

	#[test]
	fn intro_merges_for_hu_en() {
		let all = serde_json::json!({ "invoice": { "hu": " Szia ", "en": "Hi" }, "storno": { "hu": "", "en": "Sorry" } });
		assert_eq!(intro(&all, MailKind::Invoice, "hu"), "Szia");
		assert_eq!(intro(&all, MailKind::Invoice, "hu-en"), "Szia\n\nHi");
		assert_eq!(intro(&all, MailKind::Storno, "hu-en"), "Sorry");
		assert_eq!(intro(&serde_json::Value::Null, MailKind::Storno, "en"), "");
	}

	#[test]
	fn the_subject_prefix_marks_test_mode_only() {
		assert_eq!(subject_prefix(false, "hu"), "");
		assert_eq!(subject_prefix(true, "en"), "[TEST] ");
		assert_eq!(subject_prefix(true, "hu-en"), "[TESZT/TEST] ");
		assert_eq!(subject_prefix(true, "hu"), "[TESZT] ");
	}

	#[test]
	fn the_intro_refuses_links_length_and_stray_keys() {
		let ok = serde_json::json!({ "invoice": { "hu": "a", "en": "b" }, "storno": { "hu": "", "en": "" } });
		check_intro(&ok).unwrap();
		let mut link = ok.clone();
		for bad in ["see www.x.hu", "ftp://x", "mailto:a@b.hu", "at //host", "HTTPS://x.hu"] {
			link["invoice"]["en"] = bad.into();
			assert!(check_intro(&link).is_err(), "{bad}");
		}
		let mut long = ok.clone();
		long["storno"]["hu"] = "é".repeat(1001).into();
		assert!(check_intro(&long).is_err());
		let mut stray = ok.clone();
		stray["other"] = serde_json::json!({});
		assert!(check_intro(&stray).is_err());
		assert!(check_intro(&serde_json::json!({ "invoice": { "hu": "a", "en": "b" } })).is_err());
	}

	#[test]
	fn decide_applies_the_caps_in_order() {
		use crate::store::ResendCaps;
		let new = |manual: bool, to: &str, cc: usize| NewMail {
			uid: mintworks_core::ids::MailId::generate(),
			org_id: 1,
			invoice_id: 1,
			kind: MailKind::Invoice,
			recipient: to.into(),
			cc: (0..cc).map(|i| format!("c{i}@x.hu")).collect(),
			manual,
			at: Timestamp(1000),
		};
		let resend = Some(ResendCaps { gap_start: Timestamp(500), max: 2 });
		let auto = MailCaps { day_start: Timestamp(0), daily_max: 3, resend: None };
		let man = MailCaps { resend, ..auto.clone() };
		let none = MailCounts::default();
		let ok = Some((MailStatus::Queued, None));
		let exists = MailCounts { exists: true, ..none.clone() };
		assert!(matches!(
			decide(&new(false, "a@x.hu", 0), &auto, &exists),
			Err(MailQueued::Exists)
		));
		assert_eq!(decide(&new(true, "a@x.hu", 0), &man, &exists).ok(), ok);
		let maxed = MailCounts { resend_n: 2, ..none.clone() };
		assert!(matches!(
			decide(&new(true, "a@x.hu", 0), &man, &maxed),
			Err(MailQueued::ResendLimit)
		));
		let recent = MailCounts { resend_last: Some(600), ..none.clone() };
		assert!(matches!(decide(&new(true, "", 0), &man, &recent), Err(MailQueued::ResendLimit)));
		assert_eq!(
			decide(&new(false, "", 0), &auto, &none).ok(),
			Some((MailStatus::NoEmail, None))
		);
		assert_eq!(decide(&new(false, "a@x.hu", 2), &auto, &none).ok(), ok);
		let one = MailCounts { day_recipients: 1, ..none.clone() };
		let full = Some((MailStatus::Failed, Some(MailReason::DailyLimit)));
		assert_eq!(decide(&new(false, "a@x.hu", 2), &auto, &one).ok(), full);
		assert!(matches!(decide(&new(true, "a@x.hu", 2), &man, &one), Err(MailQueued::DailyLimit)));
		assert_eq!(decide(&new(true, "a@x.hu", 1), &man, &one).ok(), ok);
	}

	#[test]
	fn a_send_job_status_settles_the_row() {
		assert_eq!(settled("DONE"), Some((MailStatus::Sent, None)));
		assert_eq!(settled("RUNNING"), None);
		assert_eq!(settled("PENDING"), None);
		assert_eq!(settled("FAILED"), Some((MailStatus::Failed, Some(MailReason::SendFailed))));
		assert_eq!(settled("CANCELLED"), Some((MailStatus::Failed, Some(MailReason::SendFailed))));
	}
}

// vim: ts=4
