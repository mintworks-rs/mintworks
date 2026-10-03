//! Self-service data export and erasure.
//!
//! **Deletion is anonymization.** Hungarian accounting law keeps invoices for 8 years from the end
//! of the year they were issued (Számv. tv. 169. §), and GDPR Art. 17(3)(b) makes that legal
//! obligation an exception to erasure, so an erasure request may not reach an invoice row.
//!
//! What that leaves is a **closed column allowlist**:
//!
//! - `accounts` — `email` replaced with a placeholder that preserves uniqueness, `name`,
//!   `pwd_hash` and `pending_ref_id` nulled, `status = 'ANONYMIZED'`, `anonymized_at` stamped,
//!   `token_epoch` bumped so every live token dies; a `refs.email` naming the address takes the
//!   same placeholder.
//! - `totp_credentials` — the row is deleted; `api_keys` — every key revoked.
//! - `billing_parties` where `kind = 'P'` — `name`, `postcode`, `city`, `street`, `email`.
//! - `orgs.name` where `kind = 'PERSONAL'` — an invited account's personal org is named with
//!   its **full address** (`org::add_member`) and a self-registered one with the local
//!   part, so the column is personal data and the export dumps it.
//! - `objects` under that personal org — `body` blanked, and the `object_index` rows derived
//!   from it dropped. An ext blob or a script object is not the invoice document, so no
//!   retention floor reaches it. Blanking it is what classifies it as personal data, so the
//!   export hands the same rows over under the same scope.
//!
//! These four personal-only scopes are the same restriction for the same reason: an
//! organisation this account merely owns is other people's data on both paths — it is
//! neither erased by an erasure nor handed over by an export.
//!
//! `agent_runs` the account started, in any org — `spec` (prompt vars) blanked to `{}`, `error`
//! and `account_id` nulled, and the run's `agent_run_events` (prompts, deltas, tool arguments
//! and results) deleted; the run row stays for the cost audit, as `llm_usage` does.
//!
//! Never touched, by this path or any other: `invoices`, `invoice_lines`,
//! `invoice_vat_groups`, `invoice_documents`, `nav_submissions`, `nav_submission_xml`,
//! `payments`, `payment_allocations`, the frozen `buyer_*` snapshot columns, `consents`
//! (evidence for legal claims) and `audit_logs` (append-only). The allowlist lives in one
//! place — [`ERASURE`] below — so no caller and no store adapter can widen it.

use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::header::CONTENT_DISPOSITION;
use axum::response::{IntoResponse, Response};
use saas_core::app::App;
use saas_core::ctx::Ctx;
use saas_core::prelude::*;
use serde::{Deserialize, Serialize};

use crate::service_api::Auth;
use crate::store::ExportScope::{Account, AccountId, MemberOrg, PersonalOrg, PersonalOrgInvoice};
use crate::store::{AccountStatus, ErasurePlan, ExportSection, Scale};

/// The `GET /api/account/export` document: every section, its scope, and the closed column
/// allowlist behind it. The store returns one JSON array per entry and [`document`] puts the
/// keys back on — so what leaves the database is decided here, not by whatever columns a
/// deployment's schema happens to carry.
///
/// `id` never appears; an INTEGER `*_id` is exported as the referenced `uid`. Omitted on
/// purpose: `accounts.pwd_hash`, `token_epoch`, `failed_logins` and `locked_until`,
/// `api_keys.key_hash` (key material), `consents.legal_doc_id` (`legal_docs` has no `uid`,
/// and `doc_version`/`doc_sha256` identify the document the subject can act on), and
/// `audit_logs.account_id`/`org_id` (the subject's own ids, restated).
pub(crate) const EXPORT: &[ExportSection] = &[
	ExportSection {
		key: "accounts",
		table: "accounts",
		scope: Account,
		columns: &[
			"uid",
			"email",
			"name",
			"locale",
			"status",
			"activated_at",
			"last_login_at",
			"anonymized_at",
			"created_at",
		],
		scaled: &[],
		mask: &[],
	},
	ExportSection {
		key: "orgs",
		table: "orgs",
		scope: MemberOrg,
		// No `owner_account_id`: on an org the subject merely belongs to that renders as
		// another person's public id, which a subject access request may not hand over.
		columns: &["uid", "kind", "name", "billing_currency", "status", "created_at"],
		scaled: &[],
		mask: &[],
	},
	ExportSection {
		key: "memberships",
		table: "memberships",
		scope: AccountId,
		columns: &["org_id", "account_id", "role", "accepted_at", "created_at"],
		scaled: &[],
		mask: &[],
	},
	ExportSection {
		key: "consents",
		table: "consents",
		scope: AccountId,
		columns: &[
			"account_id",
			"org_id",
			"kind",
			"doc_version",
			"doc_sha256",
			"granted",
			"at",
			"ip",
			"user_agent",
			"withdrawn_at",
		],
		scaled: &[],
		mask: &[],
	},
	ExportSection {
		key: "billingParties",
		table: "billing_parties",
		scope: PersonalOrg,
		columns: &[
			"uid",
			"org_id",
			"kind",
			"name",
			"country",
			"tax_number",
			"eu_vat_id",
			"group_tax_no",
			"postcode",
			"city",
			"street",
			"email",
			"is_default",
			"created_at",
			"updated_at",
		],
		scaled: &[],
		mask: &[],
	},
	// Erasure blanks these bodies, which classifies them as personal data; Art. 15 and Art. 20
	// then reach exactly the same rows. `type` and `uid` come along because a bare body says
	// nothing about what it extends. No `id`: an internal row id, and the paging cursor.
	ExportSection {
		key: "objects",
		table: "objects",
		scope: PersonalOrg,
		columns: &["type", "uid", "body", "created_at", "updated_at"],
		scaled: &[],
		mask: &[],
	},
	ExportSection {
		key: "invoices",
		table: "invoices",
		scope: PersonalOrg,
		columns: &[
			"uid",
			"request_id",
			"org_id",
			"seller_id",
			"billing_party_id",
			"kind",
			"status",
			"series_code",
			"series_year",
			"number",
			"issued_at",
			"fulfilment_date",
			"due_date",
			"payment_method",
			"original_invoice_id",
			"modification_index",
			"currency",
			"rate_e6",
			"rate_date",
			"rate_source",
			"huf_rate_e6",
			"net",
			"vat",
			"gross",
			"paid_amount",
			"paid_at",
			"vat_note",
			"notes",
			"discount_kind",
			"discount_value",
			"buyer_kind",
			"buyer_name",
			"buyer_country",
			"buyer_tax_number",
			"buyer_eu_vat_id",
			"buyer_group_tax_no",
			"buyer_postcode",
			"buyer_city",
			"buyer_street",
			"created_at",
			"updated_at",
		],
		// Verbatim on purpose: `rate_e6`/`huf_rate_e6` are 1e6-scaled and `vat_rate_bp` basis
		// points, none of them amounts, and `discount_value` is typed by its sibling
		// `discount_kind` — which one `Scale` cannot describe.
		scaled: &[
			("net", Scale::Money),
			("vat", Scale::Money),
			("gross", Scale::Money),
			("paid_amount", Scale::Money),
		],
		mask: &[],
	},
	ExportSection {
		key: "invoiceLines",
		table: "invoice_lines",
		scope: PersonalOrgInvoice,
		columns: &[
			"invoice_id",
			"line_no",
			"service_id",
			"description",
			"unit",
			"qty",
			"unit_price",
			"discount_kind",
			"discount_value",
			"discount_amount",
			"discount_description",
			"net",
			"vat_code",
			"vat_rate_bp",
			"vat",
			"gross",
		],
		scaled: &[
			("qty", Scale::Qty),
			("unit_price", Scale::Money),
			("discount_amount", Scale::Money),
			("net", Scale::Money),
			("vat", Scale::Money),
			("gross", Scale::Money),
		],
		mask: &[],
	},
	ExportSection {
		key: "invoiceVatGroups",
		table: "invoice_vat_groups",
		scope: PersonalOrgInvoice,
		columns: &[
			"invoice_id",
			"vat_code",
			"vat_rate_bp",
			"net",
			"vat",
			"gross",
			"net_huf",
			"vat_huf",
			"gross_huf",
		],
		scaled: &[
			("net", Scale::Money),
			("vat", Scale::Money),
			("gross", Scale::Money),
			("net_huf", Scale::MoneyHuf),
			("vat_huf", Scale::MoneyHuf),
			("gross_huf", Scale::MoneyHuf),
		],
		mask: &[],
	},
	// `payments` has no DDL yet — `saas-billing` and its migration are plan
	// `saas-6-payments`. The key stays so the document's shape does not change when the
	// table arrives; the column list is what that plan fills in.
	ExportSection {
		key: "payments",
		table: "payments",
		scope: PersonalOrg,
		columns: &[],
		scaled: &[],
		mask: &[],
	},
	// `AccountId`, not `PersonalOrg`: erasure revokes every key the person holds, so the
	// export has to list the organisation-scoped ones it will revoke.
	ExportSection {
		key: "apiKeys",
		table: "api_keys",
		scope: AccountId,
		columns: &[
			"uid",
			"org_id",
			"account_id",
			"name",
			"prefix",
			"scopes",
			"last_used_at",
			"expires_at",
			"revoked_at",
			"created_at",
		],
		scaled: &[],
		mask: &[],
	},
	// `AccountId`, like `apiKeys`: a passkey is the person's, not an org's, and erasure deletes it.
	// `credential_id`/`credential` are deliberately absent — the public key is not personal data
	// and the serialized credential is unusable without the private half.
	ExportSection {
		key: "passkeys",
		table: "webauthn_credentials",
		scope: AccountId,
		columns: &["name", "created_at", "last_used_at"],
		scaled: &[],
		mask: &[],
	},
	ExportSection {
		key: "auditLog",
		table: "audit_logs",
		scope: AccountId,
		columns: &["at", "ip", "entity", "entity_id", "action", "detail", "request_id"],
		scaled: &[],
		// `entity_id` is a third party's uid on `membership` and the target's on an operator's
		// `account` entry; everywhere else it is the subject's own org or invoice. Blanking
		// it wholesale left `ORG_DELETED` and every `saas-invoice` action as stubs. The `entity`
		// vocabulary is `tenant` on rows older than the org refactor (`saas_core::audit`).
		mask: &[("entity_id", "entity NOT IN ('membership', 'account')")],
	},
	ExportSection {
		key: "refUses",
		table: "ref_uses",
		scope: AccountId,
		columns: &["ref_id", "at"],
		scaled: &[],
		mask: &[],
	},
	ExportSection {
		key: "usage",
		table: "usage",
		scope: AccountId,
		columns: &["key", "amount", "at"],
		scaled: &[],
		mask: &[],
	},
	// No `agent_run_events`: they are the derived stream of the thread's messages, which
	// `saas_agent::AgentHook::export` already hands over.
	ExportSection {
		key: "agentRuns",
		table: "agent_runs",
		scope: AccountId,
		columns: &["uid", "role", "status", "spec", "error", "created_at"],
		scaled: &[],
		mask: &[],
	},
];

/// The erasure allowlist, and the only place it exists. Every column here is named in this
/// module's prose above; the store is free to refuse anything not listed.
pub(crate) const ERASURE: ErasurePlan = ErasurePlan {
	accounts: &[("name", None), ("pwd_hash", None), ("pending_ref_id", None)],
	// The personal org's name is personal data: `org::add_member` names it with the
	// invitee's full address, and self-registration with the local part.
	orgs: &[("name", Some("[erased]"))],
	// `name` is NOT NULL, so it takes the marker rather than NULL.
	billing_parties: &[
		("name", Some("[erased]")),
		("postcode", None),
		("city", None),
		("street", None),
		("email", None),
	],
	// An opaque JSON body has no columns to blank, so the entry is the whole of it. The value is
	// **bound**, not interpolated, so it is the two characters an empty JSON object is and not a
	// quoted SQL literal — `'{}'` would store text no `json_extract` can read. Non-empty is what
	// turns the scrub on: the store drops the `object_index` rows in the same transaction.
	objects: &[("body", Some("{}"))],
	agent_runs: &[("spec", Some("{}")), ("error", None), ("account_id", None)],
	// Both are account-scoped credential tables, and the account row is anonymized rather than
	// deleted, so no `ON DELETE CASCADE` ever reaches them: an erased account keeping a live
	// passkey would still authenticate.
	delete_by_account: &["totp_credentials", "webauthn_credentials"],
	// `Runner::complete` blanks a DONE payload, but FAILED keeps its own as the delivery
	// diagnostic — and a `SEND_EMAIL` payload carries the address and display name, so an SMTP
	// outage that exhausted `max_attempts` would otherwise survive the erasure.
	blank_job_kinds: &["SEND_EMAIL"],
};

/// The export document: [`EXPORT`]'s keys over the store's row arrays, in order, with every
/// scaled integer rendered in its wire shape — an amount is
/// `{"amount": "12500.00", "currency": "HUF"}`, a quantity a 6-decimal string.
pub(crate) fn document(rows: Vec<serde_json::Value>) -> ClResult<serde_json::Value> {
	if rows.len() != EXPORT.len() {
		return Err(Error::internal("export: the store returned the wrong number of sections"));
	}
	let mut out = serde_json::Map::new();
	for (section, mut array) in EXPORT.iter().zip(rows) {
		rescale(section, &mut array)?;
		out.insert(section.key.to_string(), array);
	}
	Ok(serde_json::Value::Object(out))
}

/// snake_case column name to the camelCase key the store emitted.
///
/// Public because the store adapter's `dump` decides the export's JSON keys with it and
/// [`rescale`] looks them back up: two copies that drifted would export money columns as raw
/// minor units inside a subject-access document, silently.
pub fn camel(col: &str) -> String {
	let mut out = String::with_capacity(col.len());
	let mut up = false;
	for c in col.chars() {
		if c == '_' {
			up = true;
		} else if up {
			out.extend(c.to_uppercase());
			up = false;
		} else {
			out.push(c);
		}
	}
	out
}

/// Replace every scaled integer in `array` with its wire rendering, in place.
///
/// A non-integer where the section declares a scale is an error rather than a pass-through:
/// the allowlist and the schema have drifted, and the store treats that class of drift as
/// loud too. A NULL stays NULL.
pub(crate) fn rescale(section: &ExportSection, array: &mut serde_json::Value) -> ClResult<()> {
	let Some(rows) = array.as_array_mut() else {
		return Ok(());
	};
	for row in rows {
		let Some(obj) = row.as_object_mut() else { continue };
		let currency = obj
			.get("currency")
			.and_then(|c| c.as_str())
			.map(|c| CurrencyCode::from_trusted(c.to_owned()));
		for (col, scale) in section.scaled {
			let key = camel(col);
			let Some(value) = obj.get(&key) else {
				return Err(Error::internal(format!("export: {}.{col} is missing", section.table)));
			};
			if value.is_null() {
				continue;
			}
			let Some(raw) = value.as_i64() else {
				return Err(Error::internal(format!(
					"export: {}.{col} is not an integer",
					section.table
				)));
			};
			let rendered = match scale {
				Scale::Qty => serde_json::json!(Qty(raw).to_decimal_string()),
				Scale::MoneyHuf => serde_json::json!(Money(raw).to_wire(&CurrencyCode::huf())),
				Scale::Money => match &currency {
					Some(c) => serde_json::json!(Money(raw).to_wire(c)),
					None => serde_json::json!(Money(raw).to_decimal_string()),
				},
			};
			obj.insert(key, rendered);
		}
	}
	Ok(())
}

/// Számv. tv. 169. § — 8 years from the end of the invoice's issue year. This is a floor:
/// nothing may purge below it.
// A constant, not `settings['retention.invoice_years']`. Promote it to a `SETTINGS`
// key when a deployment in another jurisdiction needs a different floor.
pub const RETENTION_YEARS: i64 = 8;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteRequest {
	pub confirm_email: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteResponse {
	pub status: AccountStatus,
	pub anonymized_at: Timestamp,
	pub retained_until: String,
	pub retained_because: String,
}

/// `GET /api/account/export` — every row concerning the caller, as one JSON document.
///
/// The shape is [`EXPORT`] and the assembly is [`document`]: the store returns one row array
/// per section and never sees the document. A table this deployment does not have — the
/// `saas-invoice` ones, where it is not used — comes back as `[]`.
///
/// Step-up and rate limited, like [`delete`] beside it: a whole-database read otherwise
/// reachable with nothing but a stolen 15-minute access token. The store scopes it to the
/// personal org, so an **organisation** the account merely owns — other members' rows —
/// never leaves.
pub async fn export(State(app): State<App>, ctx: Ctx) -> ClResult<Response> {
	let (uid, doc) = Auth::new(app).export_account(&ctx).await?;
	let mut resp = Json(doc).into_response();
	let disposition = format!("attachment; filename=\"export_{}.json\"", uid.as_str());
	let value = HeaderValue::from_str(&disposition)
		.map_err(|e| Error::internal(format!("content-disposition: {e}")))?;
	resp.headers_mut().insert(CONTENT_DISPOSITION, value);
	Ok(resp)
}

/// `POST /api/account/delete` — authenticated **and** step-up. Irreversible: the account can
/// never authenticate again, and the confirmation echoes back the retention floor so the
/// caller can see what was kept and why.
pub async fn delete(
	State(app): State<App>,
	ctx: Ctx,
	Json(req): Json<DeleteRequest>,
) -> ClResult<Json<DeleteResponse>> {
	let erased = Auth::new(app).erase_account(&ctx, &req.confirm_email).await?;
	Ok(Json(DeleteResponse {
		status: erased.status,
		anonymized_at: erased.anonymized_at,
		retained_until: erased.retained_until,
		retained_because: erased.retained_because,
	}))
}

/// The last day of the year the retention floor expires in, counted from the year of
/// **erasure**. The statutory floor runs from the last invoice's issue year, which is never
/// later, so this is an over-estimate and never below it. Derived from the RFC 3339 rendering
/// rather than a date library, because the year is the only field that matters.
pub(crate) fn retained_until(at: Timestamp) -> ClResult<String> {
	let iso = at.to_rfc3339().ok_or_else(|| Error::internal("timestamp out of range"))?;
	let year: i64 = iso
		.get(..4)
		.and_then(|y| y.parse().ok())
		.ok_or_else(|| Error::internal("timestamp has no year"))?;
	Ok(format!("{}-12-31", year + RETENTION_YEARS))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn retention_floor_is_the_end_of_the_eighth_year() {
		// 2026-03-06T14:22:31Z + 8 years, rounded up to the year's end.
		assert_eq!(retained_until(Timestamp(1_772_806_951)).unwrap_or_default(), "2034-12-31");
	}
}

// vim: ts=4
