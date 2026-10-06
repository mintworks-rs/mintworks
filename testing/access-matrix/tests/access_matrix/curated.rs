// SPDX-License-Identifier: MPL-2.0
//! The curated layer: rows the level layers' oracle cannot express, each judged against its own
//! `expect`. This file is the truth.

use axum::http::{HeaderValue, Method};
use mintworks_auth::store::{AuthStore, NewWebauthnCredential, OrgKind};
use mintworks_billing::provider::PaymentState;
use mintworks_billing::store::{BillingStore, NewPayment};
use mintworks_core::prelude::*;
use mintworks_invoice::store::PartyPatch;
use mintworks_invoice::{InvoiceStore, PartyKind};
use serde_json::json;

use crate::fixture::{Fixture, call, fixture, on_rt, req};
use crate::levels::{Caller, caller, listed};
use crate::objects::{ObjKind, OrgTag, account, make};
use crate::oracle::{Expect, Outcome};
use crate::report::Report;
use crate::routes::routes;
use crate::subjects::{Subject, account_of, special};

/// The object a row acts on. Every mutating row gets a fresh one, so a wrongly allowed delete
/// never takes a canonical object (or org) away from the other layers.
#[derive(Clone, Copy, Debug)]
pub enum O {
	/// `A:Kind` / `B:Kind`: the canonical object (`fx.obj`).
	Canon(ObjKind, OrgTag),
	/// `new O:Kind`. For `Org`, a child of O owned by O's owner.
	New(ObjKind, OrgTag),
	/// `new child:Org`: a SHARED child of A whose owner is a stranger, so A's roles are inherited.
	Child,
	/// `cred(X)`: a WebAuthn credential row of X's account; [`OWN`] is the caller's.
	Cred(&'static str),
	/// `new A:IssuedInvoice + a payment on it`; the listing is searched for the payment.
	Paid,
	/// A live QR session from `POST /api/auth/qr/init`.
	Qr,
	/// `qr?`: a session id nobody issued.
	QrUnknown,
}

pub const OWN: &str = "own";

/// The request body: the route table's, or one the row needs to say something specific.
#[derive(Clone, Copy, Debug)]
pub enum B {
	Table,
	/// `switch-org {X}`.
	Switch(OrgTag),
	/// member_a's *access* token as `refreshToken`.
	RefreshWithAccess,
	/// A consent grant that deserialises, so the request reaches the service rather than dying
	/// in `Json` extraction (a non-authz 4xx, which `classify` reads as Allow).
	Consent,
}

#[derive(Clone, Copy, Debug)]
pub struct Row {
	pub id: &'static str,
	/// A roster name, or a builder name [`special`] knows.
	pub subject: &'static str,
	/// `"METHOD /path"`, exactly as the route table spells it.
	pub route: &'static str,
	pub object: Option<O>,
	pub expect: Outcome,
	/// `Allow, listed` / `Allow, unlisted`.
	pub listed: Option<bool>,
	pub body: B,
	pub why: &'static str,
}

const fn row(
	id: &'static str,
	subject: &'static str,
	route: &'static str,
	object: Option<O>,
	expect: Outcome,
	why: &'static str,
) -> Row {
	Row { id, subject, route, object, expect, listed: None, body: B::Table, why }
}

impl Row {
	const fn listed(mut self, l: bool) -> Self {
		self.listed = Some(l);
		self
	}
	const fn body(mut self, b: B) -> Self {
		self.body = b;
		self
	}
}

const ALLOW: Outcome = Outcome::Allow;
const TOKEN: Outcome = Outcome::Deny("E-AUTH-TOKEN");
const KEY_REVOKED: Outcome = Outcome::Deny("E-AUTH-KEY-REVOKED");
const SUSPENDED: Outcome = Outcome::Deny("E-AUTH-SUSPENDED");
const SCOPE: Outcome = Outcome::Deny("E-AUTH-SCOPE");
const CONSENT: Outcome = Outcome::Deny("E-AUTH-CONSENT-REQUIRED");
const FORBIDDEN: Outcome = Outcome::Deny("E-AUTH-FORBIDDEN");
const STEPUP: Outcome = Outcome::Deny("E-AUTH-STEPUP");
const IMPOSSIBLE: Outcome = Outcome::Deny("E-AUTH-STEPUP-IMPOSSIBLE");
const NOTFOUND: Outcome = Outcome::Deny("E-CORE-NOTFOUND");

use ObjKind::{Account, ApiKey, Document, DraftInvoice, Invite, IssuedInvoice, Legal, Org};
use OrgTag::{A, A1, B as OB, S};

// Row-table shorthands for `Row.object: Option<O>`.
#[allow(clippy::unnecessary_wraps)]
const fn new(k: ObjKind, t: OrgTag) -> Option<O> {
	Some(O::New(k, t))
}
#[allow(clippy::unnecessary_wraps)]
const fn canon(k: ObjKind, t: OrgTag) -> Option<O> {
	Some(O::Canon(k, t))
}
#[allow(clippy::unnecessary_wraps)]
const fn cred(who: &'static str) -> Option<O> {
	Some(O::Cred(who))
}

const QR_STATUS: &str = "GET /api/auth/qr/{sessionId}/status";
const QR_DETAILS: &str = "GET /api/auth/qr/{sessionId}/details";
const QR_RESPOND: &str = "POST /api/auth/qr/{sessionId}/respond";
const WA_PATCH: &str = "PATCH /api/auth/wa/credentials/{credentialId}";
const WA_DELETE: &str = "DELETE /api/auth/wa/credentials/{credentialId}";
const SWITCH: &str = "POST /api/auth/switch-org";
const INVITE_ACCEPT: &str = "POST /api/org/invites/{code}/accept";
const KEY_PATCH: &str = "PATCH /api/api-keys/{uid}";
const KEY_DELETE: &str = "DELETE /api/api-keys/{uid}";

#[rustfmt::skip]
pub const ROWS: &[Row] = &[
	// 1. Seller org (invoices filed under an ancestor's seller)
	row("CU-01", "admin_a1", "POST /api/invoices", None, FORBIDDEN, "seller-admin is checked on the seller's org (A), not the acting org (§1.8 seller-admin)"),
	row("CU-02", "admin_a_on_a1", "POST /api/invoices", None, ALLOW, "A's admin reaches the seller resolved from A1's ancestors (§1.8)"),
	row("CU-03", "admin_a1", "POST /api/invoices/{uid}/issue", new(DraftInvoice, A1), FORBIDDEN, "a customer org \"reads its invoices and may not issue them\" (§1.8)"),
	row("CU-04", "admin_a_on_a1", "POST /api/invoices/{uid}/issue", new(DraftInvoice, A1), ALLOW, "the mutation resolves from the invoice's `seller_id` (§invoices)"),
	row("CU-05", "admin_a1", "PATCH /api/invoices/{uid}", new(DraftInvoice, A1), FORBIDDEN, "every invoice mutation is seller-admin (§invoices)"),
	row("CU-06", "admin_a1", "POST /api/invoices/{uid}/storno", new(IssuedInvoice, A1), FORBIDDEN, "the level rung precedes step-up, so step-up never reveals the route (D-2)"),
	row("CU-07", "member_a1", "GET /api/invoices/{uid}", new(DraftInvoice, A1), ALLOW, "reads are filtered by `org_id`, the workspace the invoice lives in (§invoices)"),
	row("CU-08", "admin_a", "POST /api/invoices/{uid}/issue", new(DraftInvoice, A1), NOTFOUND, "lookup is by the workspace `org_id` (A1), not the issuer, so acting on A does not find it (§invoices)"),
	row("CU-09", "owner_b", "GET /api/invoices/{uid}", new(DraftInvoice, A1), NOTFOUND, "cross-tenant (D-2 object)"),
	row("CU-10", "admin_a1", "GET /api/seller", None, ALLOW, "the seller is resolved by the ancestor walk (§seller)"),
	row("CU-11", "admin_a1", "PUT /api/seller", None, FORBIDDEN, "seller-admin on A (§1.8)"),
	row("CU-12", "admin_a1", "GET /api/nav/credentials", None, FORBIDDEN, "seller-admin on A (§1.8)"),
	row("CU-13", "key_a_all", "GET /api/invoices/{uid}", new(DraftInvoice, A1), NOTFOUND, "\"a key reaches no other org, ancestor or descendant\" (§api-keys)"),
	// 2. Org owner
	row("CU-14", "owner_b", "DELETE /api/orgs/{uid}", new(Org, A), NOTFOUND, "no membership on the named org (D-2 level OrgOwner)"),
	row("CU-15", "admin_a", "DELETE /api/orgs/{uid}", new(Org, A), NOTFOUND, "no direct membership on the minted child; `owner_of` does not inherit (RC-17)"),
	row("CU-16", "member_a", "DELETE /api/orgs/{uid}", new(Org, A), NOTFOUND, "same rule as CU-15"),
	row("CU-17", "admin_a_on_a1", "DELETE /api/orgs/{uid}", new(Org, A1), NOTFOUND, "same rule as CU-15"),
	row("CU-18", "owner_a", "DELETE /api/orgs/{uid}", Some(O::Child), NOTFOUND, "OWNER of the parent is not OWNER of the child: `owner_of` reads a direct membership (RC-17)"),
	row("CU-19", "operator", "DELETE /api/orgs/{uid}", new(Org, OB), NOTFOUND, "operator power is not ownership, and it has no direct membership on the child (RC-17)"),
	row("CU-20", "admin_a", "POST /api/org/transfer-ownership", None, FORBIDDEN, "not OWNER of the acting org (D-2)"),
	row("CU-21", "admin_a", "PUT /api/seller/closed", None, FORBIDDEN, "the named org is the seller's org (A); admin is not owner (Rev OrgOwner)"),
	row("CU-22", "admin_a1", "PUT /api/seller/closed", None, FORBIDDEN, "A is the seller of admin_a1's own org, so no membership there is 403 (RC-24)"),
	row("CU-23", "foreign_org_tok", "DELETE /api/orgs/{uid}", new(Org, OB), NOTFOUND, "a non-member `org` claim gains nothing; no membership on B (D-7b)"),
	row("CU-24", "foreign_org_tok", "GET /api/org", None, FORBIDDEN, "the claim resolves to `org_id = None`, giving \"no org selected\" (D-7b, `ctx.rs:145`)"),
	// 3. Step-up
	row("CU-25", "owner_a", "POST /api/api-keys", None, ALLOW, "fresh `auth_at` (§step-up)"),
	row("CU-26", "owner_a_stale", "POST /api/api-keys", None, STEPUP, "stale `auth_at`: re-authenticate (§step-up)"),
	row("CU-27", "impersonation", "POST /api/api-keys", None, IMPOSSIBLE, "an impersonation token has no `auth_at` (§9.4)"),
	row("CU-28", "key_a_all", "POST /api/api-keys", None, SCOPE, "the route is unscoped, so a key fails at the scope rung before step-up (D-2 order)"),
	row("CU-29", "no_authat_tok", "POST /api/api-keys", None, IMPOSSIBLE, "a session without `auth_at` can never pass step-up (§step-up)"),
	row("CU-30", "owner_a_stale", "POST /api/auth/step-up", None, ALLOW, "step-up is the remedy, so it cannot require step-up itself (§4.x)"),
	row("CU-31", "owner_a_stale", "GET /api/org", None, ALLOW, "staleness matters only on step-up routes (§1.8)"),
	row("CU-32", "owner_a_stale", "POST /api/invoices/{uid}/storno", new(IssuedInvoice, A), STEPUP, "storno is in the step-up set (§4 revisions)"),
	row("CU-33", "key_a_all", "POST /api/invoices/{uid}/storno", new(IssuedInvoice, A), IMPOSSIBLE, "scope passes and the key has no `auth_at` (D-2)"),
	// 4. Impersonation
	row("CU-34", "impersonation", "GET /api/auth/me", None, ALLOW, "acts as the target (§9.4)"),
	row("CU-35", "impersonation", "GET /api/org", None, ALLOW, "the target's membership on A (§9.4)"),
	row("CU-36", "impersonation", "PATCH /api/org", None, FORBIDDEN, "only the target's role (MEMBER) applies; `imp` = operator adds nothing (§9.4)"),
	row("CU-37", "impersonation", "GET /api/org/members", None, FORBIDDEN, "same rule as CU-36"),
	row("CU-38", "impersonation", "GET /api/admin/subscriptions", None, FORBIDDEN, "operator routes re-read the target's root membership (§1.8)"),
	row("CU-39", "impersonation", "POST /api/admin/accounts/{uid}/status", new(Account, A), FORBIDDEN, "level precedes step-up (D-2)"),
	row("CU-40", "impersonation", "GET /api/account/export", None, IMPOSSIBLE, "\"no step-up route is reachable while impersonating\" (§9.4)"),
	// 5. Tokens
	row("CU-41", "totp_pending", "GET /api/auth/me", None, TOKEN, "`totpToken` \"is not an access token\" (§4.2)"),
	row("CU-42", "refresh_tok", "GET /api/auth/me", None, TOKEN, "a `typ: refresh` token is not an access token (§token)"),
	row("CU-43", "anon", "POST /api/auth/refresh", None, TOKEN, "an access token is not a refresh token (§4.x refresh)").body(B::RefreshWithAccess),
	row("CU-44", "no_org_tok", "GET /api/auth/me", None, ALLOW, "`/me` needs no org"),
	row("CU-45", "no_org_tok", "GET /api/org", None, FORBIDDEN, "\"no org selected\" (`ctx.rs:145`)"),
	row("CU-46", "no_org_tok", "GET /api/invoices", None, FORBIDDEN, "same rule as CU-45, on a scoped bundle"),
	row("CU-47", "ep_tok", "GET /api/auth/me", None, TOKEN, "`ep` mismatch: \"token superseded\" (§token). No roster subject covers this (Rev no-`ep`)"),
	row("CU-48", "ghost_tok", "GET /api/auth/me", None, TOKEN, "unknown account (`auth_mw.rs`)"),
	// 6. Liveness and membership
	row("CU-49", "suspended_acct", "GET /api/auth/me", None, TOKEN, "suspension bumps `token_epoch`, so the old token is superseded (§token). The oracle says SUSPENDED (Rev no-`ep`)"),
	row("CU-50", "susp_live_tok", "GET /api/auth/me", None, SUSPENDED, "a current-epoch token still meets the status check (`auth_mw.rs`)"),
	row("CU-51", "removed_a", "GET /api/auth/me", None, ALLOW, "the account is live; only the membership is gone"),
	row("CU-52", "removed_a", "GET /api/org", None, FORBIDDEN, "membership re-read from the DB, giving no acting org (§1.8, Rev liveness)"),
	row("CU-53", "removed_a", "GET /api/invoices", None, FORBIDDEN, "same rule as CU-52"),
	row("CU-54", "suspended_org_member", "GET /api/auth/me", None, ALLOW, "the account is live"),
	row("CU-55", "suspended_org_member", "GET /api/org", None, FORBIDDEN, "a suspended org gives no acting org (Rev liveness)"),
	row("CU-56", "suspended_org_member", SWITCH, None, SUSPENDED, "\"a SUSPENDED org: E-AUTH-SUSPENDED\" (§switch-org)").body(B::Switch(S)),
	row("CU-57", "invited_a", SWITCH, None, NOTFOUND, "an un-accepted invite is not a reaching membership (§switch-org)").body(B::Switch(A)),
	row("CU-58", "invited_a", "GET /api/org", None, ALLOW, "login lands on its own PERSONAL org (RC-05)"),
	row("CU-59", "member_a", SWITCH, None, NOTFOUND, "\"never 403\" (§switch-org)").body(B::Switch(OB)),
	row("CU-60", "member_a", SWITCH, None, ALLOW, "a membership on an ancestor reaches the child (§switch-org)").body(B::Switch(A1)),
	// 7. Consent
	row("CU-61", "no_consent", "GET /api/auth/me", None, ALLOW, "consent-exempt (§consent)"),
	row("CU-62", "no_consent", "POST /api/auth/step-up", None, ALLOW, "consent-exempt"),
	row("CU-63", "no_consent", "GET /api/consents", None, ALLOW, "consent-exempt"),
	row("CU-64", "fresh(no_consent)", "POST /api/consents", None, ALLOW, "consent-exempt (this is how consent is given)"),
	row("CU-65", "no_consent", "GET /api/account/export", None, ALLOW, "consent-exempt (GDPR)"),
	row("CU-66", "fresh(no_consent)", "POST /api/account/delete", None, ALLOW, "consent-exempt (GDPR)"),
	row("CU-67", "no_consent", "GET /api/org", None, CONSENT, "gated"),
	row("CU-68", "no_consent", "GET /api/orgs", None, CONSENT, "gated"),
	row("CU-69", "no_consent", "POST /api/auth/password", None, CONSENT, "gated (and denied before the handler runs, so the shared subject is safe to use)"),
	row("CU-70", "no_consent", "GET /api/invoices", None, CONSENT, "gated scoped bundle"),
	row("CU-71", "no_consent", "GET /api/documents/{uid}", canon(Document, A), ALLOW, "documents are not consent-gated (D-7d; ruled 2026-10-06)"),
	row("CU-72", "no_consent", "GET /api/plans/offers", None, ALLOW, "public"),
	// 8. API keys
	row("CU-73", "key_a_all", "GET /api/auth/me", None, SCOPE, "`/api/auth/*` is unscoped (§api-keys)"),
	row("CU-74", "key_a_all", "GET /api/invoices", None, ALLOW, "`invoice:read`; keys skip the consent rung (D-7a: the server answers TOKEN)"),
	row("CU-75", "key_a_read", "GET /api/invoices", None, ALLOW, "`invoice:read` (D-7a)"),
	row("CU-76", "key_a_read", "POST /api/billing-parties", None, SCOPE, "`invoice:write` is missing"),
	row("CU-77", "key_a_none", "GET /api/invoices", None, SCOPE, "no scopes"),
	row("CU-78", "key_revoked", "GET /api/invoices", None, KEY_REVOKED, "revoked (§errCodes)"),
	row("CU-79", "key_revoked", "GET /api/auth/me", None, KEY_REVOKED, "liveness precedes scope (D-2 order)"),
	row("CU-80", "key_a_all", "GET /api/invoices/{uid}", canon(DraftInvoice, OB), NOTFOUND, "cross-tenant"),
	row("CU-81", "key_operator", "GET /api/admin/subscriptions", None, SCOPE, "an unscoped operator route"),
	row("CU-82", "key_operator", "POST /api/admin/payments", None, IMPOSSIBLE, "a root admin's key is operator-grade, but a key never satisfies step-up (api-surface §API keys)"),
	row("CU-83", "key_a_all", "POST /api/api-keys", None, SCOPE, "a key cannot mint a key (unscoped)"),
	// 9. List visibility
	row("CU-84", "member_a1", "GET /api/invoices", new(DraftInvoice, A1), ALLOW, "workspace filter: A1's own invoices (§invoices)").listed(true),
	row("CU-85", "member_a1", "GET /api/invoices", canon(DraftInvoice, A), ALLOW, "A's invoices are not A1's workspace").listed(false),
	row("CU-86", "member_a", "GET /api/invoices", new(DraftInvoice, A1), ALLOW, "`org_id` filter, not subtree, not issuer (§invoices)").listed(false),
	row("CU-87", "admin_a_on_a1", "GET /api/invoices", new(DraftInvoice, A1), ALLOW, "acting org A1").listed(true),
	row("CU-88", "admin_a_on_a1", "GET /api/invoices", canon(DraftInvoice, A), ALLOW, "acting org A1").listed(false),
	row("CU-89", "operator", "GET /api/invoices", canon(DraftInvoice, OB), ALLOW, "operator acts on A; the org-member list is filtered by org (§invoices)").listed(false),
	row("CU-90", "member_a", "GET /api/invoices/{uid}/payments", Some(O::Paid), ALLOW, "found through the allocation link row (§payments). The level layer records no presence for this route").listed(true),
	// 10. Routes skipped by the level layers: path params without an `ObjKind`
	row("CU-91", "anon", QR_STATUS, Some(O::Qr), ALLOW, "public"),
	row("CU-92", "anon", QR_STATUS, Some(O::QrUnknown), NOTFOUND, "an unknown session (`qr.rs`)"),
	row("CU-93", "member_a", QR_DETAILS, Some(O::Qr), ALLOW, "authenticated"),
	row("CU-94", "anon", QR_DETAILS, Some(O::Qr), TOKEN, "authenticated"),
	row("CU-95", "key_a_all", QR_DETAILS, Some(O::Qr), SCOPE, "`/api/auth/*` is unscoped"),
	row("CU-96", "member_a", QR_DETAILS, Some(O::QrUnknown), NOTFOUND, "an unknown session"),
	row("CU-97", "member_a", QR_RESPOND, Some(O::Qr), ALLOW, "authenticated"),
	row("CU-98", "anon", QR_RESPOND, Some(O::Qr), TOKEN, "authenticated"),
	row("CU-99", "key_a_all", QR_RESPOND, Some(O::Qr), SCOPE, "unscoped"),
	row("CU-100", "no_consent", QR_RESPOND, Some(O::Qr), ALLOW, "listed consent-exempt in the route table"),
	row("CU-101", "member_a", WA_PATCH, cred("member_a"), ALLOW, "the caller's own credential"),
	row("CU-102", "member_a1", WA_PATCH, cred("member_a"), NOTFOUND, "another account's credential (cross-account rule)"),
	row("CU-103", "anon", WA_PATCH, cred("member_a"), TOKEN, "authenticated"),
	row("CU-104", "key_a_all", WA_PATCH, cred("admin_a"), SCOPE, "unscoped"),
	row("CU-105", "owner_a", WA_DELETE, cred("owner_a"), ALLOW, "own credential, fresh step-up"),
	row("CU-106", "owner_a", WA_DELETE, cred("member_a"), NOTFOUND, "another account's credential"),
	row("CU-107", "owner_a_stale", WA_DELETE, cred("owner_a"), STEPUP, "stale"),
	row("CU-108", "no_consent", WA_DELETE, cred("no_consent"), CONSENT, "gated"),
	row("CU-109", "key_a_all", WA_DELETE, cred("admin_a"), SCOPE, "unscoped"),
	// 11. `self_mut` cells skipped by the level layers. A denied row runs on the roster subject;
	// an `Allow` row would mutate the account, so it runs on `fresh(X)`.
	row("CU-110", "fresh(impersonation)", "POST /api/consents", None, FORBIDDEN, "an impersonator may not do self-service writes for the target (policy: `Ctx::impersonated` doc)").body(B::Consent),
	row("CU-111", "fresh(impersonation)", "POST /api/auth/password", None, FORBIDDEN, "an impersonator may not do self-service writes for the target (policy: `Ctx::impersonated` doc)"),
	row("CU-112", "fresh(impersonation)", "DELETE /api/consents/{kind}", canon(Legal, A), FORBIDDEN, "an impersonator may not do self-service writes for the target (policy: `Ctx::impersonated` doc)"),
	row("CU-113", "impersonation", "POST /api/account/delete", None, IMPOSSIBLE, "step-up (§9.4)"),
	row("CU-114", "impersonation", "DELETE /api/orgs/{uid}", canon(Org, A), FORBIDDEN, "MEMBER of A, not OWNER; level precedes step-up"),
	row("CU-115", "impersonation", "POST /api/org/transfer-ownership", None, FORBIDDEN, "same rule as CU-114"),
	row("CU-116", "impersonation", "POST /api/auth/totp", None, IMPOSSIBLE, "\"can never arm a second factor\" (§rev 11)"),
	row("CU-117", "impersonation", "DELETE /api/auth/totp", None, IMPOSSIBLE, "step-up"),
	row("CU-118", "impersonation", "POST /api/auth/totp/verify", None, IMPOSSIBLE, "step-up"),
	row("CU-119", "impersonation", "POST /api/auth/wa/register/challenge", None, IMPOSSIBLE, "step-up"),
	row("CU-120", "impersonation", "POST /api/auth/wa/register", None, IMPOSSIBLE, "step-up"),
	row("CU-121", "impersonation", WA_DELETE, cred("member_a"), IMPOSSIBLE, "step-up"),
	// removed_a and suspended_org_member: live account, fresh `auth_at`, no acting org and no
	// role anywhere. Ids in pairs (removed_a, suspended_org_member).
	row("CU-122", "fresh(removed_a)", "POST /api/consents", None, ALLOW, "account-level, needs no org"),
	row("CU-123", "fresh(suspended_org_member)", "POST /api/consents", None, ALLOW, "account-level, needs no org"),
	row("CU-124", "fresh(removed_a)", "POST /api/auth/password", None, ALLOW, "account-level"),
	row("CU-125", "fresh(suspended_org_member)", "POST /api/auth/password", None, ALLOW, "account-level"),
	row("CU-126", "fresh(removed_a)", "DELETE /api/consents/{kind}", canon(Legal, A), ALLOW, "account-level"),
	row("CU-127", "fresh(suspended_org_member)", "DELETE /api/consents/{kind}", canon(Legal, A), ALLOW, "account-level"),
	row("CU-128", "fresh(removed_a)", "POST /api/account/delete", None, ALLOW, "GDPR self-service survives losing the org"),
	row("CU-129", "fresh(suspended_org_member)", "POST /api/account/delete", None, ALLOW, "GDPR self-service survives losing the org"),
	row("CU-130", "removed_a", "DELETE /api/orgs/{uid}", new(Org, A), NOTFOUND, "no live membership on the named org (D-2 OrgOwner)"),
	row("CU-131", "suspended_org_member", "DELETE /api/orgs/{uid}", new(Org, S), NOTFOUND, "no live membership on the named org (D-2 OrgOwner)"),
	row("CU-132", "removed_a", "POST /api/org/transfer-ownership", None, NOTFOUND, "the body names A; `owner_of` finds no live membership there (RC-07)"),
	row("CU-133", "suspended_org_member", "POST /api/org/transfer-ownership", None, NOTFOUND, "the body names A; `owner_of` finds no live membership there (RC-07)"),
	row("CU-134", "fresh(removed_a)", "POST /api/auth/totp", None, ALLOW, "account-level, fresh step-up"),
	row("CU-135", "fresh(suspended_org_member)", "POST /api/auth/totp", None, ALLOW, "account-level, fresh step-up"),
	row("CU-136", "fresh(removed_a)", "DELETE /api/auth/totp", None, ALLOW, "account-level (no TOTP enrolled yields a non-authz 4xx)"),
	row("CU-137", "fresh(suspended_org_member)", "DELETE /api/auth/totp", None, ALLOW, "account-level (no TOTP enrolled yields a non-authz 4xx)"),
	row("CU-138", "fresh(removed_a)", "POST /api/auth/totp/verify", None, ALLOW, "account-level (a wrong code yields a non-authz 4xx)"),
	row("CU-139", "fresh(suspended_org_member)", "POST /api/auth/totp/verify", None, ALLOW, "account-level (a wrong code yields a non-authz 4xx)"),
	row("CU-140", "fresh(removed_a)", "POST /api/auth/wa/register/challenge", None, ALLOW, "account-level"),
	row("CU-141", "fresh(suspended_org_member)", "POST /api/auth/wa/register/challenge", None, ALLOW, "account-level"),
	row("CU-142", "fresh(removed_a)", "POST /api/auth/wa/register", None, ALLOW, "account-level (a bogus attestation yields a non-authz 4xx)"),
	row("CU-143", "fresh(suspended_org_member)", "POST /api/auth/wa/register", None, ALLOW, "account-level (a bogus attestation yields a non-authz 4xx)"),
	row("CU-144", "fresh(removed_a)", WA_DELETE, cred(OWN), ALLOW, "account-level"),
	row("CU-145", "fresh(suspended_org_member)", WA_DELETE, cred(OWN), ALLOW, "account-level"),
	// Self-service writes and token mints refuse an impersonator before any lookup.
	row("CU-146", "fresh(impersonation)", "POST /api/auth/step-up", None, FORBIDDEN, "an impersonator may not do self-service writes or token mints for the target (policy: `Ctx::impersonated` doc)"),
	row("CU-147", "fresh(impersonation)", SWITCH, None, FORBIDDEN, "an impersonator may not do self-service writes or token mints for the target (policy: `Ctx::impersonated` doc)").body(B::Switch(A)),
	row("CU-148", "fresh(impersonation)", "POST /api/orgs", None, FORBIDDEN, "an impersonator may not do self-service writes or token mints for the target (policy: `Ctx::impersonated` doc)"),
	row("CU-149", "fresh(impersonation)", INVITE_ACCEPT, new(Invite, A), FORBIDDEN, "an impersonator may not do self-service writes or token mints for the target (policy: `Ctx::impersonated` doc)"),
	row("CU-150", "fresh(impersonation)", KEY_PATCH, new(ApiKey, A), FORBIDDEN, "an impersonator may not do self-service writes or token mints for the target (policy: `Ctx::impersonated` doc)"),
	row("CU-151", "fresh(impersonation)", KEY_DELETE, new(ApiKey, A), FORBIDDEN, "an impersonator may not do self-service writes or token mints for the target (policy: `Ctx::impersonated` doc)"),
	row("CU-152", "fresh(impersonation)", WA_PATCH, cred(OWN), FORBIDDEN, "an impersonator may not do self-service writes or token mints for the target (policy: `Ctx::impersonated` doc)"),
	row("CU-153", "fresh(impersonation)", QR_RESPOND, Some(O::Qr), FORBIDDEN, "an impersonator may not do self-service writes or token mints for the target (policy: `Ctx::impersonated` doc)"),
	// The dead-member pairs for the same routes, as the oracle derives them.
	row("CU-154", "removed_a", "POST /api/auth/step-up", None, ALLOW, "account-level, needs no org"),
	row("CU-155", "suspended_org_member", "POST /api/auth/step-up", None, ALLOW, "account-level, needs no org"),
	row("CU-156", "removed_a", SWITCH, None, NOTFOUND, "a removed membership is not a reaching membership (§switch-org)").body(B::Switch(A)),
	row("CU-157", "fresh(removed_a)", "POST /api/orgs", None, ALLOW, "account-level: any live account may found an org"),
	row("CU-158", "fresh(suspended_org_member)", "POST /api/orgs", None, ALLOW, "account-level: any live account may found an org"),
	row("CU-159", "fresh(removed_a)", INVITE_ACCEPT, new(Invite, A), ALLOW, "a bearer invite is accepted by whoever holds the code"),
	row("CU-160", "fresh(suspended_org_member)", INVITE_ACCEPT, new(Invite, A), ALLOW, "a bearer invite is accepted by whoever holds the code"),
	row("CU-161", "removed_a", KEY_PATCH, new(ApiKey, A), FORBIDDEN, "no acting org (Rev liveness)"),
	row("CU-162", "suspended_org_member", KEY_PATCH, new(ApiKey, A), FORBIDDEN, "no acting org (Rev liveness)"),
	row("CU-163", "removed_a", KEY_DELETE, new(ApiKey, A), FORBIDDEN, "no acting org (Rev liveness)"),
	row("CU-164", "suspended_org_member", KEY_DELETE, new(ApiKey, A), FORBIDDEN, "no acting org (Rev liveness)"),
	row("CU-165", "fresh(removed_a)", WA_PATCH, cred(OWN), ALLOW, "account-level"),
	row("CU-166", "fresh(suspended_org_member)", WA_PATCH, cred(OWN), ALLOW, "account-level"),
	row("CU-167", "removed_a", QR_RESPOND, Some(O::Qr), ALLOW, "account-level (a wrong match code yields a non-authz 4xx)"),
	row("CU-168", "suspended_org_member", QR_RESPOND, Some(O::Qr), ALLOW, "account-level (a wrong match code yields a non-authz 4xx)"),
	row("CU-169", "key_unconsented", "GET /api/invoices", None, ALLOW, "a key skips the consent gate even when its owner owes consent (consent::gate)"),
];

/// What a row's object resolved to.
struct Target {
	name: String,
	/// Substituted into the path's parameter.
	key: String,
	/// What the listing is searched for (the payment, for [`O::Paid`]).
	look_for: String,
	obj: Option<crate::objects::Obj>,
	qr_secret: Option<String>,
}

fn unique() -> u32 {
	static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
	N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// A1 has no default billing party, and an A1 draft bills `Party::OrgDefault`.
async fn a1_party(fx: &Fixture) {
	static DONE: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
	DONE.get_or_init(async || {
		let patch = PartyPatch {
			kind: Some(PartyKind::Company),
			name: Some("A1 Vevo Kft.".into()),
			country: Some("HU".into()),
			tax_number: Patch::Value("11111111242".into()),
			postcode: Patch::Value("1052".into()),
			city: Patch::Value("Budapest".into()),
			street: Patch::Value("Deak ter 3.".into()),
			email: Patch::Value("a1@matrix.invalid".into()),
			is_default: Some(true),
			..Default::default()
		};
		fx.store.create_party(fx.orgs.a1.id, &patch).await.unwrap();
	})
	.await;
}

fn plain(name: String, key: String) -> Target {
	Target { name, look_for: key.clone(), key, obj: None, qr_secret: None }
}

async fn target(fx: &'static Fixture, o: O, caller: &Subject) -> Target {
	let caller = caller.clone();
	on_rt(async move {
		let from = |obj: crate::objects::Obj| Target {
			name: obj.name.clone(),
			key: obj.key.clone(),
			look_for: obj.key.clone(),
			obj: Some(obj),
			qr_secret: None,
		};
		match o {
			O::Canon(k, t) => from(fx.obj(k, t).clone()),
			O::New(k, t) => {
				if t == A1 {
					a1_party(fx).await;
				}
				from(make(fx, k, t).await)
			}
			O::Child => {
				let n = unique();
				let mail = format!("child-owner-{n}@matrix.invalid");
				let stranger = account(&fx.store, &fx.pwd_hash, &mail).await;
				let name = format!("child-{n}");
				let org =
					fx.store.create_org(OrgKind::Shared, fx.orgs.a.id, &name, stranger.id, None);
				plain("child:Org".into(), org.await.unwrap().uid.into_string())
			}
			O::Cred(who) => {
				let owner = if who == OWN { caller } else { fx.subject(who).clone() };
				let acct = account_of(fx, &owner).await;
				let id = format!("matrix-cred-{}", unique());
				let new = NewWebauthnCredential {
					account_id: acct.id,
					credential_id: id.clone(),
					credential: "{}".into(),
					name: "matrix".into(),
					created_at: Timestamp::now(),
				};
				fx.store.put_webauthn_credential(&new, 100).await.unwrap().unwrap();
				plain(format!("cred({})", owner.name), id)
			}
			O::Paid => {
				let inv = make(fx, IssuedInvoice, A).await;
				let id: i64 = sqlx::query_scalar("SELECT id FROM invoices WHERE uid = ?")
					.bind(&inv.key)
					.fetch_one(fx.store.read_pool())
					.await
					.unwrap();
				let pay = fx
					.store
					.create_payment(&NewPayment {
						org_id: inv.org_id,
						kind: "manual".into(),
						provider: Some("stub".into()),
						provider_ref: Some(format!("matrix-paid-{}", unique())),
						request_id: None,
						status: PaymentState::Pending,
						amount: Money(100_000),
						currency: CurrencyCode::huf(),
						ext_ref: None,
						note: None,
						created_by: None,
						invoice_id: Some(id),
					})
					.await
					.unwrap();
				Target { look_for: pay.uid.into_string(), ..from(inv) }
			}
			O::Qr => {
				let r = call(&fx.router, req(Method::POST, "/api/auth/qr/init", None, None)).await;
				let b = r.body.unwrap_or_else(|| panic!("qr/init answered {}", r.status));
				let key = b["sessionId"].as_str().unwrap().to_owned();
				let secret = b["secret"].as_str().unwrap().to_owned();
				Target { qr_secret: Some(secret), ..plain("qr".into(), key) }
			}
			O::QrUnknown => plain("qr?".into(), "0".repeat(32)),
		}
	})
	.await
}

fn fill(path: &str, key: Option<&str>) -> String {
	path.split('/')
		.map(|seg| match key {
			_ if seg == "{lineNo}" => "1",
			Some(k) if seg.starts_with('{') => k,
			_ => seg,
		})
		.collect::<Vec<_>>()
		.join("/")
}

/// Every `self_mut` cell the level layer skips must be a curated row, or nothing tests it.
pub async fn self_mut_is_curated() {
	let fx = fixture().await;
	let mut missing = Vec::new();
	for r in routes().iter().filter(|r| r.self_mut) {
		let route = format!("{} {}", r.method, r.path);
		for s in fx.subjects.iter().filter(|s| matches!(caller(r, s), Caller::Skip)) {
			let fresh = format!("fresh({})", s.name);
			let hit = ROWS
				.iter()
				.any(|row| row.route == route && (row.subject == s.name || row.subject == fresh));
			if !hit {
				missing.push(format!("{route} × {}", s.name));
			}
		}
	}
	assert!(missing.is_empty(), "self_mut cells with no curated row:\n  {}", missing.join("\n  "));
}

/// Every row, serially: most of them mutate, and each builds its own subject and object.
pub async fn curated() {
	let fx = fixture().await;
	let specs = routes();
	let mut report = Report::new("curated", ROWS.len());
	for row in ROWS {
		let (method, path) = row.route.split_once(' ').unwrap();
		let method = Method::from_bytes(method.as_bytes()).unwrap();
		let spec = specs
			.iter()
			.find(|r| r.method == method && r.path == path)
			.unwrap_or_else(|| panic!("{}: {} is not in routes()", row.id, row.route));
		let s = match fx.subjects.iter().find(|s| s.name == row.subject) {
			Some(s) => s.clone(),
			None => special(fx, row.subject).await,
		};
		let t = match row.object {
			Some(o) => Some(target(fx, o, &s).await),
			None => None,
		};
		let body = match row.body {
			B::Table => (spec.body)(t.as_ref().and_then(|t| t.obj.as_ref()).unwrap_or(&fx.objs[0])),
			B::Switch(to) => Some(json!({ "orgUid": fx.orgs.of(to).uid })),
			B::RefreshWithAccess => Some(json!({ "refreshToken": fx.subject("member_a").bearer })),
			B::Consent => Some(json!({ "kind": "TOS", "version": "x" })),
		};
		let uri = fill(path, t.as_ref().map(|t| t.key.as_str()));
		let bearer = s.bearer.as_deref();
		let mut rq = req(method, &uri, bearer, body);
		if let Some(secret) = t.as_ref().and_then(|t| t.qr_secret.as_deref()) {
			rq.headers_mut().insert("x-qr-secret", HeaderValue::from_str(secret).unwrap());
		}
		let resp = call(&fx.router, rq).await;
		let obj = t.as_ref().map(|t| t.name.as_str());
		let seen = match (&t, row.listed) {
			(Some(t), Some(_)) if resp.status.is_success() => {
				match listed(fx, &uri, bearer, &resp, &t.look_for).await {
					Ok(b) => Some(b),
					Err(e) => {
						report.error(row.route, &e, row.subject, obj);
						continue;
					}
				}
			}
			_ => None,
		};
		let exp = Expect { outcome: row.expect, rule: row.id, listed: row.listed };
		report.check(row.route, row.subject, obj, &exp, &resp, seen);
	}
	report.finish();
}

// vim: ts=4
