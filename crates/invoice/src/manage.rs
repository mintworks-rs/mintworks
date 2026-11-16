// SPDX-License-Identifier: MPL-2.0
//! Managing an invoice after issue: Javítás ([`Invoices::correct`]), the issue options ([`Invoices::issue_opts`]), the timeline
//! ([`Invoices::timeline`]) and the org's "needs attention" counts ([`Invoices::counts`]).
//!
//! Timeline items and refusal reasons are codes; the client owns the text.

use mintworks_core::ctx::Ctx;
use mintworks_core::prelude::*;
use mintworks_core::store::Role;
use serde::Serialize;
use serde_json::{Value, json};

use crate::draft::{Line, NewDraft, Party};
use crate::filing::FilingSummary;
use crate::numbering::{self, MAX_FULFILMENT_DRIFT_DAYS};
use crate::service_api::{Invoices, conflict, invoice_discount, storno_blocked};
use crate::store::{Invoice, InvoiceCounts, InvoiceMail, InvoiceStatus, MailStatus, PaymentMethod};
use crate::storno::StornoCheck;

/// What [`Invoices::issue_opts`] takes. `send: None` follows the seller org's
/// `invoice.mail.auto`; `Some(false)` also opts the invoice (and its storno) out of mail. An
/// expected date (`YYYY-MM-DD`) that differs from what issuing would produce now refuses with
/// `409 E-INV-DATES-CHANGED`.
#[derive(Clone, Debug, Default)]
pub struct IssueOpts {
	pub send: Option<bool>,
	pub expected_fulfilment: Option<String>,
	pub expected_due: Option<String>,
}

/// [`Invoices::correct`]'s answer.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Correction {
	pub storno_uid: String,
	pub draft_uid: String,
}

/// One timeline entry. `at` serializes as RFC 3339; `ref` is the linked invoice's uid on the
/// `STORNO` / `REPLACED` items; `verdict` is the filing verdict on the `NAV` item.
#[derive(Clone, Debug, Serialize)]
pub struct TimelineItem {
	pub at: Timestamp,
	pub kind: &'static str,
	pub code: String,
	pub vars: Value,
	#[serde(rename = "ref")]
	pub reference: Option<String>,
	pub verdict: Option<String>,
}

/// [`Invoices::timeline`]'s answer: items oldest first. An app extends it by pushing its own.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Timeline {
	pub items: Vec<TimelineItem>,
	pub can_storno: StornoCheck,
	/// What the last comparison with NAV found, when one ran.
	pub nav_diff: Option<Value>,
}

impl Timeline {
	/// Inserts after every item stamped at or before `at`, so equal stamps keep their push
	/// order. A missing `at` is skipped.
	pub fn push(
		&mut self,
		at: Option<Timestamp>,
		kind: &'static str,
		code: impl Into<String>,
		vars: Value,
		reference: Option<String>,
		verdict: Option<String>,
	) {
		let Some(at) = at else { return };
		let i = self.items.partition_point(|x| x.at <= at);
		self.items
			.insert(i, TimelineItem { at, kind, code: code.into(), vars, reference, verdict });
	}
}

impl Invoices {
	/// Javítás: storno `uid` and open a replacement draft prefilled from it, linked through
	/// `replaces_invoice_id`. Seller Admin, plus step-up when a storno is still to be made.
	///
	/// Not one transaction — the storno queues NAV work. The draft goes first and is
	/// idempotent on `requestId = "correct:{uid}"`, so a retry replays it and finds the storno
	/// already made (an original already `STORNOED` skips the gate and keeps its storno). A
	/// storno that fails deletes the draft again.
	pub async fn correct(&self, ctx: &Ctx, uid: &str, reason: &str) -> ClResult<Correction> {
		let original = self.invoice(ctx, uid).await?;
		let seller = self.seller_of_invoice(&original).await?;
		// Before the draft: a member's replay of `requestId` would hand back an admin's draft,
		// which the failed storno below would then delete.
		self.require_seller_role(ctx, &seller, Role::Admin).await?;
		let store = self.store()?;
		let existing = match original.status {
			InvoiceStatus::Stornoed => store.storno_of(original.id).await?,
			_ => None,
		};
		if existing.is_none() {
			// Refused up front, so neither a step-up nor a gate refusal leaves a draft behind.
			// Step-up first, as `storno` orders it: a stale session must not learn the gate.
			mintworks_core::auth_mw::require_stepup(&self.app, ctx).await?;
			if let Some(reason) = self.storno_gate(ctx, &original).await.reason {
				return Err(storno_blocked(reason));
			}
		}

		let draft = self.draft(ctx, &self.correction_draft(&original).await?).await?;
		let cancelled = match existing {
			Some(st) => {
				// A retry finishes a mail an earlier attempt missed; queued at most once.
				if let Err(e) = self.mail_auto(ctx, st.uid.as_str()).await {
					tracing::error!(invoice = %st.uid.as_str(), error = %e, "storno mail not queued");
				}
				st
			}
			None => match self.storno(ctx, uid, reason).await {
				Ok(st) => st,
				Err(e) => {
					// A concurrent `correct` that won the storno shares this replayed draft.
					if matches!(store.storno_of(original.id).await, Ok(None))
						&& let Err(d) = self.delete_draft(ctx, draft.uid.as_str()).await
					{
						tracing::error!(invoice = %draft.uid.as_str(), error = %d, "Javítás draft not deleted");
					}
					return Err(e);
				}
			},
		};
		// A replayed draft may already be linked, or issued since (then the link stands as is).
		if draft.status == InvoiceStatus::Draft
			&& draft.replaces_invoice_id != Some(original.id)
			&& let Err(e) = store.set_replaces(draft.id, original.id).await
		{
			// Issued or deleted since the read: the storno has committed, so not a 500.
			let now = store.invoice_by_id(draft.id).await?;
			if now.is_some_and(|d| d.status == InvoiceStatus::Draft) {
				return Err(e);
			}
		}
		Ok(Correction {
			storno_uid: cancelled.uid.as_str().to_owned(),
			draft_uid: draft.uid.as_str().to_owned(),
		})
	}

	/// The replacement draft: party, currency, payment method, series, period, notes, the
	/// invoice discount, and every line as an ad-hoc line with its discount, note and goods
	/// flag. The due date takes the draft default; the fulfilment date per [`keeps_fulfilment`].
	async fn correction_draft(&self, original: &Invoice) -> ClResult<NewDraft> {
		let store = self.store()?;
		let billing_party = match original.billing_party_id {
			Some(id) => match store.party_by_id(id).await? {
				Some(p) if p.org_id == original.org_id => Party::Uid(p.uid),
				_ => Party::OrgDefault,
			},
			None => Party::OrgDefault,
		};
		let mut lines = Vec::new();
		for l in store.invoice_lines(original.id).await? {
			let mut line = Line::adhoc(l.description, l.unit, l.qty, l.unit_price, l.vat_code);
			line.discount = crate::money::discount_of(l.discount_kind, l.discount_value)?;
			line.discount_description = l.discount_description;
			line.note = l.note;
			line.product = Some(l.product);
			lines.push(line);
		}
		Ok(NewDraft {
			request_id: Some(format!("correct:{}", original.uid.as_str())),
			billing_party,
			lines,
			discount: invoice_discount(original)?,
			// CARD is set only by the payment flow, and a draft refuses it from a caller.
			payment_method: match original.payment_method {
				m @ (PaymentMethod::Transfer | PaymentMethod::Cash) => Some(m),
				_ => None,
			},
			currency: Some(original.currency.clone()),
			fulfilment_date: keeps_fulfilment(original, &numbering::resolved_today()?)?
				.then(|| original.fulfilment_date.clone())
				.flatten(),
			due_date: None,
			period_start: original.period_start.clone(),
			period_end: original.period_end.clone(),
			notes: original.notes.clone(),
			series_code: original.series_code.clone(),
			doc_lang: original.doc_lang.clone(),
		})
	}

	/// [`Self::issue_with`] behind the expected-date guard. The guard runs only while the
	/// invoice is still open: an already-issued one returns unchanged, as `issue` does.
	pub async fn issue_opts(&self, ctx: &Ctx, uid: &str, opts: &IssueOpts) -> ClResult<Invoice> {
		let expected = [opts.expected_fulfilment.as_deref(), opts.expected_due.as_deref()];
		for d in expected.into_iter().flatten() {
			numbering::check_date(d)?;
		}
		if expected.iter().any(Option::is_some) {
			let inv = self.invoice(ctx, uid).await?;
			if matches!(inv.status, InvoiceStatus::Draft | InvoiceStatus::Pending) {
				let plan = self.issue_plan(ctx, uid).await?;
				if expected[0].is_some_and(|d| d != plan.fulfilment_date)
					|| expected[1].is_some_and(|d| d != plan.due_date)
				{
					return Err(conflict(
						"E-INV-DATES-CHANGED",
						"the invoice dates changed; review them before issuing",
					));
				}
			}
		}
		self.issue_with(ctx, uid, opts.send).await
	}

	/// The invoice's history, oldest first: `CREATED`, `ISSUED`, the NAV filing and its last
	/// comparison, each mail, `PAID`, the storno links and the Javítás links. Any member, like
	/// the invoice itself. A filing probe error reads as no filing, so a NAV outage never
	/// breaks the read. `PAID` comes from `paid_at` (core has no audit-log read): one item, at
	/// full payment.
	pub async fn timeline(&self, ctx: &Ctx, uid: &str) -> ClResult<Timeline> {
		let inv = self.invoice(ctx, uid).await?;
		let store = self.store()?;
		let (filing, can_storno) = self.filing_and_gate(ctx, &inv).await;
		let mut t = Timeline {
			items: Vec::new(),
			can_storno,
			nav_diff: filing.as_ref().and_then(|f| f.adopt_diff.clone()),
		};
		t.push(Some(inv.created_at), "CREATED", "CREATED", json!({}), None, None);
		if let Some(n) = &inv.number {
			t.push(inv.issued_at, "ISSUED", "ISSUED", json!({ "number": n }), None, None);
		}
		if let Some(f) = &filing {
			let (code, vars) = nav_text(f);
			t.push(
				Some(f.done_at.unwrap_or(f.created_at)),
				"NAV",
				code,
				vars,
				None,
				f.verdict.clone(),
			);
			// Its own kind: a client banner looks up `kind == "NAV"`.
			if let Some(r) = &f.adopt_result {
				let n = f.adopt_diff.as_ref().and_then(|d| d.get("total")).and_then(Value::as_i64);
				let vars = json!({ "n": n.unwrap_or(0) });
				t.push(f.adopt_checked_at, "NAV_CHECK", format!("NAV_CHECK_{r}"), vars, None, None);
			}
		}
		// Newest first from the store; walked backwards so equal stamps keep their order.
		for m in store.mail_rows(inv.org_id, inv.id).await?.iter().rev() {
			let (code, vars) = mail_text(m);
			t.push(Some(m.at), "EMAIL", code, vars, None, None);
		}
		t.push(inv.paid_at, "PAID", "PAID", json!({}), None, None);
		let org = inv.org_id;
		let number = |i: &Invoice| json!({ "number": i.number.clone().unwrap_or_default() });
		let uid_of = |i: &Invoice| Some(i.uid.as_str().to_owned());
		if inv.status == InvoiceStatus::Stornoed
			&& let Some(st) = store.storno_of(inv.id).await?
		{
			t.push(st.issued_at, "STORNO", "STORNOED_BY", number(&st), uid_of(&st), None);
		}
		if let Some(id) = inv.original_invoice_id
			&& let Some(o) = store.invoice_by_id(id).await?.filter(|o| o.org_id == org)
		{
			t.push(inv.issued_at, "STORNO", "STORNOES", number(&o), uid_of(&o), None);
		}
		for d in store.replaced_by(inv.id).await?.into_iter().filter(|d| d.org_id == org) {
			t.push(Some(d.created_at), "REPLACED", "REPLACED_BY", number(&d), uid_of(&d), None);
		}
		if let Some(id) = inv.replaces_invoice_id
			&& let Some(o) = store.invoice_by_id(id).await?.filter(|o| o.org_id == org)
		{
			t.push(Some(inv.created_at), "REPLACED", "REPLACES", number(&o), uid_of(&o), None);
		}
		Ok(t)
	}

	/// The acting org's "needs attention" counts in one aggregate. `emailProblem` uses the same
	/// `invoice.mail.problem_days` window as `?email=problem`. A Member of the seller's org.
	pub async fn counts(&self, ctx: &Ctx) -> ClResult<InvoiceCounts> {
		let seller = self.seller_of_org(ctx).await?;
		self.require_seller_role(ctx, &seller, Role::Member).await?;
		let now = Timestamp::now();
		let today = numbering::date_of(now)?;
		let days = crate::mail::org_int(&self.app, ctx.org()?, "invoice.mail.problem_days").await?;
		self.store()?
			.invoice_counts(ctx.org()?, &today, Some(Timestamp(now.0 - days * 86_400)))
			.await
	}
}

/// Whether a Javítás draft keeps the original's fulfilment date: not CASH (that must be
/// today), no settlement period (that derives its own), and within the drift window — one
/// day short of it, as the draft is checked against a later today.
fn keeps_fulfilment(inv: &Invoice, today: &str) -> ClResult<bool> {
	let Some(f) = inv.fulfilment_date.as_deref() else { return Ok(false) };
	if inv.payment_method == PaymentMethod::Cash || inv.period_end.is_some() {
		return Ok(false);
	}
	let drift = (numbering::parse(today)? - numbering::parse(f)?).whole_days();
	Ok(drift.abs() < MAX_FULFILMENT_DRIFT_DAYS)
}

/// The NAV item's code and vars. A HELD filing NAV rejected (a number it
/// already holds) is `NAV_HELD`, `NAV_HELD_ACK` once acknowledged; one with no verdict but a
/// parked fault is `NAV_ATTENTION`.
fn nav_text(f: &FilingSummary) -> (&'static str, Value) {
	let error = || {
		json!({
			"error": f.error_code.clone().unwrap_or_default(),
			"msg": f.error_msg.clone().unwrap_or_default(),
		})
	};
	let verdict = f.verdict.as_deref();
	if f.state.as_deref() == Some("HELD") && verdict == Some("REJECTED") {
		let code = if f.resolved_at.is_some() { "NAV_HELD_ACK" } else { "NAV_HELD" };
		return (code, json!({ "result": f.adopt_result }));
	}
	match verdict {
		Some("DONE") => ("NAV_DONE", json!({})),
		Some("WARN") => ("NAV_WARN", json!({})),
		Some("REJECTED") => ("NAV_REJECTED", error()),
		Some("FAILED") => ("NAV_FAILED", error()),
		_ if f.error_code.is_some() => ("NAV_ATTENTION", error()),
		_ => ("NAV_PENDING", json!({})),
	}
}

/// The EMAIL item's code and vars.
fn mail_text(m: &InvoiceMail) -> (&'static str, Value) {
	match m.status {
		MailStatus::Sent => ("MAIL_SENT", json!({ "recipient": m.recipient })),
		MailStatus::Queued => ("MAIL_QUEUED", json!({ "recipient": m.recipient })),
		MailStatus::NoEmail => ("MAIL_NO_EMAIL", json!({})),
		MailStatus::Failed => ("MAIL_FAILED", json!({ "reason": m.reason })),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn push_is_stable_and_skips_missing_stamps() {
		let mut t = Timeline {
			items: Vec::new(),
			can_storno: StornoCheck { ok: true, reason: None },
			nav_diff: None,
		};
		t.push(Some(Timestamp(20)), "B", "B1", json!({}), None, None);
		t.push(Some(Timestamp(10)), "A", "A", json!({}), None, None);
		t.push(Some(Timestamp(20)), "B", "B2", json!({}), None, None);
		t.push(None, "X", "X", json!({}), None, None);
		let codes: Vec<_> = t.items.iter().map(|i| i.code.as_str()).collect();
		assert_eq!(codes, ["A", "B1", "B2"]);
	}
}

// vim: ts=4
