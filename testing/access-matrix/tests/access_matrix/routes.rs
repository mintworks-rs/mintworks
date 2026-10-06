// SPDX-License-Identifier: MPL-2.0
//! Who may call what: one row per mounted method×path of every framework route bundle.
//!
//! **This table is the truth**: a new or changed route, level, scope or step-up edits a row here
//! in the same change. Where the intended policy and the code disagree, the row follows the
//! policy and the oracle reports the mismatch.

use axum::http::Method;
use serde_json::{Value, json};

use crate::fixture::{Fixture, fixture_now};
use crate::objects::{Obj, ObjKind, OrgTag};

/// `api-surface.md` §1.8; step-up is the separate `stepup` modifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
	Public,
	Authenticated,
	OrgMember,
	/// ADMIN/OWNER on the acting org (the token's `org` claim).
	OrgAdmin,
	/// ADMIN/OWNER on the org owning the seller the acting org files under.
	SellerAdmin,
	/// OWNER of the org the route names.
	OrgOwner,
	/// ADMIN/OWNER on the root org.
	Operator,
}

pub struct RouteSpec {
	pub method: Method,
	pub path: &'static str,
	pub level: Level,
	pub stepup: bool,
	/// The bundle's `ScopePrefix`; `None` fails every API key closed.
	pub scope: Option<&'static str>,
	pub consent_gated: bool,
	/// The kind whose `Obj::key` fills the path's parameter.
	pub object: Option<ObjKind>,
	/// The kind a listing returns.
	pub lists: Option<ObjKind>,
	/// Mutates the caller itself: run it as a disposable clone.
	pub self_mut: bool,
	pub body: fn(&Obj) -> Option<Value>,
}

// The signature is `RouteSpec::body`'s fn-pointer type.
#[allow(clippy::unnecessary_wraps)]
fn no_body(_: &Obj) -> Option<Value> {
	None
}

#[allow(clippy::unnecessary_wraps)]
fn empty_obj(_: &Obj) -> Option<Value> {
	Some(json!({}))
}

impl RouteSpec {
	fn stepup(mut self) -> Self {
		self.stepup = true;
		self
	}
	fn scope(mut self, s: &'static str) -> Self {
		self.scope = Some(s);
		self
	}
	fn gated(mut self) -> Self {
		self.consent_gated = true;
		self
	}
	fn obj(mut self, k: ObjKind) -> Self {
		self.object = Some(k);
		self
	}
	fn lists(mut self, k: ObjKind) -> Self {
		self.lists = Some(k);
		self
	}
	fn self_mut(mut self) -> Self {
		self.self_mut = true;
		self
	}
	fn body(mut self, f: fn(&Obj) -> Option<Value>) -> Self {
		self.body = f;
		self
	}
}

// Bodies that get past `Json<T>` extraction: a 422 rejection precedes the handler's authz.
#[allow(clippy::unnecessary_wraps)] // `RouteSpec::body`'s fn-pointer type
fn mint_body(_: &Obj) -> Option<Value> {
	Some(json!({ "name": "matrix", "scopes": ["invoice:read"] }))
}
#[allow(clippy::unnecessary_wraps)] // `RouteSpec::body`'s fn-pointer type
fn rename_body(_: &Obj) -> Option<Value> {
	Some(json!({ "name": "renamed" }))
}
#[allow(clippy::unnecessary_wraps)] // `RouteSpec::body`'s fn-pointer type
fn nav_creds_body(_: &Obj) -> Option<Value> {
	Some(json!({ "login": "l", "techPassword": "p", "signKey": "s", "exchangeKey": "e" }))
}
#[allow(clippy::unnecessary_wraps)] // `RouteSpec::body`'s fn-pointer type
fn pay_body(_: &Obj) -> Option<Value> {
	Some(json!({ "provider": "stub", "returnUrl": "https://app.invalid/back" }))
}
fn allocation(fx: &Fixture) -> Value {
	let inv = &fx.obj(ObjKind::IssuedInvoice, OrgTag::A).key;
	json!({ "invoiceUid": inv, "amount": { "amount": "1", "currency": "HUF" } })
}
#[allow(clippy::unnecessary_wraps)] // `RouteSpec::body`'s fn-pointer type
fn allocation_body(_: &Obj) -> Option<Value> {
	Some(allocation(fixture_now()))
}
#[allow(clippy::unnecessary_wraps)] // `RouteSpec::body`'s fn-pointer type
fn manual_payment_body(_: &Obj) -> Option<Value> {
	let fx = fixture_now();
	Some(json!({
		"orgUid": fx.orgs.a.uid,
		"kind": "BANK_TRANSFER",
		"amount": { "amount": "1", "currency": "HUF" },
		"receivedAt": "2026-01-01T00:00:00Z",
	}))
}

/// Names A with A's own OWNER as the target, so an allowed call is a no-op 4xx, not a transfer.
#[allow(clippy::unnecessary_wraps)] // `RouteSpec::body`'s fn-pointer type
fn transfer_body(_: &Obj) -> Option<Value> {
	let fx = fixture_now();
	Some(json!({ "orgUid": fx.orgs.a.uid, "accountUid": fx.subject("owner_a").account_uid }))
}
#[allow(clippy::unnecessary_wraps)] // `RouteSpec::body`'s fn-pointer type
fn wa_register_body(_: &Obj) -> Option<Value> {
	let response = json!({ "attestationObject": "AA", "clientDataJSON": "AA" });
	let registration =
		json!({ "id": "AA", "rawId": "AA", "type": "public-key", "response": response });
	Some(json!({ "blob": "x", "registration": registration }))
}
#[allow(clippy::unnecessary_wraps)] // `RouteSpec::body`'s fn-pointer type
fn service_body(_: &Obj) -> Option<Value> {
	let price = json!({ "amount": "100.00", "currency": "HUF" });
	Some(
		json!({ "code": "X1", "name": "Thing", "unit": "db", "unitPrice": price, "vatCode": "STD27" }),
	)
}
#[allow(clippy::unnecessary_wraps)] // `RouteSpec::body`'s fn-pointer type
fn line_body(_: &Obj) -> Option<Value> {
	let price = json!({ "amount": "1", "currency": "HUF" });
	Some(
		json!({ "qty": "1", "description": "x", "unit": "db", "unitPrice": price, "vatCode": "STD27" }),
	)
}
#[allow(clippy::unnecessary_wraps)] // `RouteSpec::body`'s fn-pointer type
fn quote_body(_: &Obj) -> Option<Value> {
	Some(json!({ "offer": fixture_now().obj(ObjKind::Offer, OrgTag::A).key }))
}

fn r(method: Method, path: &'static str, level: Level) -> RouteSpec {
	// Placeholder bodies; the policy layers fill the ones a route needs to get past validation.
	let body = if method == Method::GET || method == Method::DELETE { no_body } else { empty_obj };
	RouteSpec {
		method,
		path,
		level,
		stepup: false,
		scope: None,
		consent_gated: false,
		object: None,
		lists: None,
		self_mut: false,
		body,
	}
}

pub fn routes() -> Vec<RouteSpec> {
	use Level::*;
	use Method as M;
	use ObjKind as K;
	#[allow(unused_mut)]
	let mut v = vec![
		// core health (`into_service`)
		r(M::GET, "/healthz", Public),
		r(M::GET, "/readyz", Public),
		// auth::public
		r(M::POST, "/api/auth/logout", Public),
		r(M::GET, "/api/pow/challenge", Public),
		r(M::POST, "/api/auth/register", Public),
		r(M::POST, "/api/auth/activate", Public),
		r(M::POST, "/api/auth/resend-activation", Public),
		r(M::POST, "/api/auth/login", Public),
		r(M::POST, "/api/auth/login/totp", Public),
		r(M::GET, "/api/auth/wa/login/challenge", Public),
		r(M::POST, "/api/auth/wa/login", Public),
		r(M::POST, "/api/auth/qr/init", Public),
		r(M::GET, "/api/auth/qr/{sessionId}/status", Public),
		r(M::POST, "/api/auth/refresh", Public),
		r(M::POST, "/api/auth/password/reset-request", Public),
		r(M::POST, "/api/auth/password/reset", Public),
		r(M::GET, "/api/legal/{kind}", Public).obj(K::Legal),
		// auth::authenticated, consent-exempt
		r(M::GET, "/api/auth/me", Authenticated),
		r(M::POST, "/api/auth/step-up", Authenticated).self_mut(),
		r(M::GET, "/api/consents", Authenticated),
		r(M::POST, "/api/consents", Authenticated).self_mut(),
		r(M::GET, "/api/account/export", Authenticated).stepup(),
		r(M::POST, "/api/account/delete", Authenticated)
			.body(|_| Some(json!({ "confirmEmail": "x@matrix.invalid" })))
			.stepup()
			.self_mut(),
		r(M::DELETE, "/api/orgs/{uid}", OrgOwner).stepup().obj(K::Org).self_mut(),
		r(M::POST, "/api/org/transfer-ownership", OrgOwner)
			.body(transfer_body)
			.stepup()
			.self_mut(),
		r(M::GET, "/api/api-keys", OrgMember).lists(K::ApiKey),
		r(M::GET, "/api/api-keys/scopes", OrgMember),
		r(M::PATCH, "/api/api-keys/{uid}", OrgMember)
			.obj(K::ApiKey)
			.body(rename_body)
			.self_mut(),
		r(M::DELETE, "/api/api-keys/{uid}", OrgMember).obj(K::ApiKey).self_mut(),
		r(M::GET, "/api/auth/wa/credentials", Authenticated),
		r(M::PATCH, "/api/auth/wa/credentials/{credentialId}", Authenticated)
			.body(rename_body)
			.self_mut(),
		r(M::GET, "/api/auth/qr/{sessionId}/details", Authenticated),
		r(M::POST, "/api/auth/qr/{sessionId}/respond", Authenticated)
			.body(|_| Some(json!({ "approved": false, "matchCode": "00" })))
			.self_mut(),
		// auth::authenticated, consent-gated
		r(M::POST, "/api/auth/password", Authenticated)
			.body(|_| {
				Some(json!({ "currentPassword": "wrong", "newPassword": "Another-Long-Passw0rd!" }))
			})
			.gated()
			.self_mut(),
		r(M::POST, "/api/auth/totp", Authenticated).stepup().gated().self_mut(),
		r(M::DELETE, "/api/auth/totp", Authenticated).stepup().gated().self_mut(),
		r(M::POST, "/api/auth/totp/verify", Authenticated)
			.body(|_| Some(json!({ "code": "000000" })))
			.stepup()
			.gated()
			.self_mut(),
		r(M::POST, "/api/auth/switch-org", Authenticated).gated().self_mut(),
		r(M::GET, "/api/orgs", Authenticated).gated().lists(K::Org),
		r(M::POST, "/api/orgs", Authenticated)
			.body(|_| Some(json!({ "name": "matrix-org" })))
			.gated()
			.self_mut(),
		r(M::GET, "/api/org", OrgMember).gated(),
		r(M::PATCH, "/api/org", OrgAdmin).gated(),
		r(M::GET, "/api/org/members", OrgAdmin).gated().lists(K::Member),
		r(M::POST, "/api/org/members", OrgAdmin)
			.body(|_| Some(json!({ "email": "m@matrix.invalid", "role": "MEMBER" })))
			.gated(),
		r(M::DELETE, "/api/org/members", OrgAdmin)
			.body(|_| Some(json!({ "email": "m@matrix.invalid" })))
			.gated(),
		r(M::GET, "/api/org/invites", OrgAdmin).gated().lists(K::Invite),
		r(M::POST, "/api/org/invites/{code}/accept", Authenticated)
			.gated()
			.obj(K::Invite)
			.self_mut(),
		// "per `auth.invite_by`": the default is encoded; the oracle owns the setting.
		r(M::POST, "/api/auth/signup-refs", OrgAdmin).gated(),
		r(M::PATCH, "/api/org/members/{accountUid}", OrgAdmin)
			.body(|_| Some(json!({ "role": "MEMBER" })))
			.gated()
			.obj(K::Member),
		r(M::DELETE, "/api/org/members/{accountUid}", OrgAdmin).gated().obj(K::Member),
		r(M::DELETE, "/api/consents/{kind}", Authenticated)
			.gated()
			.obj(K::Legal)
			.self_mut(),
		r(M::POST, "/api/api-keys", OrgMember).stepup().gated().body(mint_body),
		r(M::POST, "/api/auth/wa/register/challenge", Authenticated)
			.stepup()
			.gated()
			.self_mut(),
		r(M::POST, "/api/auth/wa/register", Authenticated)
			.body(wa_register_body)
			.stepup()
			.gated()
			.self_mut(),
		r(M::DELETE, "/api/auth/wa/credentials/{credentialId}", Authenticated)
			.stepup()
			.gated()
			.self_mut(),
		// auth::operator (not consent-gated)
		r(M::POST, "/api/admin/accounts/{uid}/status", Operator)
			.body(|_| Some(json!({ "status": "SUSPENDED" })))
			.stepup()
			.obj(K::Account),
		r(M::POST, "/api/admin/accounts/{uid}/revoke", Operator)
			.stepup()
			.obj(K::Account),
		// invoice::org_read
		r(M::GET, "/api/currencies", Authenticated).scope("invoice").gated(),
		r(M::GET, "/api/seller", OrgMember).scope("invoice").gated(),
		r(M::GET, "/api/services", OrgMember).scope("invoice").gated().lists(K::Service),
		r(M::GET, "/api/services/{uid}", OrgMember)
			.scope("invoice")
			.gated()
			.obj(K::Service),
		r(M::GET, "/api/billing-parties", OrgMember)
			.scope("invoice")
			.gated()
			.lists(K::Party),
		r(M::GET, "/api/billing-parties/{uid}", OrgMember)
			.scope("invoice")
			.gated()
			.obj(K::Party),
		r(M::GET, "/api/invoices", OrgMember)
			.scope("invoice")
			.gated()
			.lists(K::DraftInvoice),
		r(M::GET, "/api/invoices/{uid}", OrgMember)
			.scope("invoice")
			.gated()
			.obj(K::DraftInvoice),
		r(M::GET, "/api/invoices/{uid}/pdf", OrgMember)
			.scope("invoice")
			.gated()
			.obj(K::IssuedInvoice),
		// invoice::org_parties
		r(M::POST, "/api/billing-parties", OrgMember)
			.body(|_| Some(json!({ "kind": "C", "name": "Acme Kft", "country": "HU" })))
			.scope("invoice")
			.gated(),
		r(M::PATCH, "/api/billing-parties/{uid}", OrgMember)
			.scope("invoice")
			.gated()
			.obj(K::Party),
		r(M::DELETE, "/api/billing-parties/{uid}", OrgMember)
			.scope("invoice")
			.gated()
			.obj(K::Party),
		// invoice::org_invoices
		r(M::POST, "/api/invoices", SellerAdmin).scope("invoice").gated(),
		r(M::PATCH, "/api/invoices/{uid}", SellerAdmin)
			.scope("invoice")
			.gated()
			.obj(K::DraftInvoice),
		r(M::DELETE, "/api/invoices/{uid}", SellerAdmin)
			.scope("invoice")
			.gated()
			.obj(K::DraftInvoice),
		r(M::POST, "/api/invoices/{uid}/lines", SellerAdmin)
			.body(line_body)
			.scope("invoice")
			.gated()
			.obj(K::DraftInvoice),
		r(M::PATCH, "/api/invoices/{uid}/lines/{lineNo}", SellerAdmin)
			.scope("invoice")
			.gated()
			.obj(K::DraftInvoice),
		r(M::DELETE, "/api/invoices/{uid}/lines/{lineNo}", SellerAdmin)
			.scope("invoice")
			.gated()
			.obj(K::DraftInvoice),
		r(M::POST, "/api/invoices/{uid}/issue", SellerAdmin)
			.scope("invoice")
			.gated()
			.obj(K::DraftInvoice),
		r(M::POST, "/api/invoices/{uid}/storno", SellerAdmin)
			.stepup()
			.scope("invoice")
			.gated()
			.obj(K::IssuedInvoice),
		// invoice::org_services
		r(M::POST, "/api/services", SellerAdmin)
			.body(service_body)
			.scope("invoice")
			.gated(),
		r(M::PATCH, "/api/services/{uid}", SellerAdmin)
			.scope("invoice")
			.gated()
			.obj(K::Service),
		r(M::DELETE, "/api/services/{uid}", SellerAdmin)
			.scope("invoice")
			.gated()
			.obj(K::Service),
		// invoice::org_seller (unscoped)
		r(M::POST, "/api/seller", OrgAdmin).gated(),
		r(M::PUT, "/api/seller", SellerAdmin).gated(),
		// The doc's "owner of the seller's own org": OrgOwner, named by the seller rather than the path.
		r(M::PUT, "/api/seller/closed", OrgOwner)
			.body(|_| Some(json!({ "closed": false })))
			.stepup()
			.gated(),
		r(M::PUT, "/api/seller/payment-days", SellerAdmin).gated(),
		// nav::org_credentials (unscoped)
		r(M::GET, "/api/nav/credentials", SellerAdmin).gated(),
		r(M::PUT, "/api/nav/credentials", SellerAdmin)
			.stepup()
			.gated()
			.body(nav_creds_body),
		// billing::public / org / operator
		r(M::POST, "/api/webhook/{provider}", Public).scope("billing").obj(K::Provider),
		r(M::POST, "/api/invoices/{uid}/pay", OrgMember)
			.scope("billing")
			.gated()
			.obj(K::IssuedInvoice)
			.body(pay_body),
		r(M::GET, "/api/invoices/{uid}/payments", OrgMember)
			.scope("billing")
			.gated()
			.obj(K::IssuedInvoice)
			.lists(K::Payment),
		r(M::GET, "/api/payments", OrgMember).scope("billing").gated().lists(K::Payment),
		r(M::GET, "/api/payments/{uid}", OrgMember)
			.scope("billing")
			.gated()
			.obj(K::Payment),
		r(M::GET, "/api/payment-providers", Authenticated).scope("billing").gated(),
		r(M::POST, "/api/admin/payments", Operator)
			.stepup()
			.scope("billing")
			.gated()
			.body(manual_payment_body),
		r(M::POST, "/api/admin/payments/{uid}/allocations", Operator)
			.stepup()
			.scope("billing")
			.gated()
			.obj(K::Payment)
			.body(allocation_body),
		r(M::POST, "/api/admin/payments/{uid}/refund", Operator)
			.stepup()
			.scope("billing")
			.gated()
			.obj(K::Payment),
		// pdf::routes (not consent-gated)
		r(M::GET, "/api/documents/{uid}", OrgMember).scope("pdf").obj(K::Document),
		r(M::GET, "/api/documents/{uid}/pdf", OrgMember).scope("pdf").obj(K::Document),
		// core::refs
		r(M::GET, "/api/refs/{code}", Public).obj(K::Ref),
		r(M::POST, "/api/refs", OrgAdmin)
			.body(|_| Some(json!({ "type": "promo" })))
			.scope("refs")
			.gated(),
		r(M::GET, "/api/refs", OrgAdmin).scope("refs").gated().lists(K::Ref),
		r(M::DELETE, "/api/refs/{code}", OrgAdmin).scope("refs").gated().obj(K::Ref),
		r(M::POST, "/api/refs/{code}/reactivate", OrgAdmin)
			.scope("refs")
			.gated()
			.obj(K::Ref),
		// entitle::routes
		r(M::GET, "/api/entitlements", OrgMember).scope("entitlements").gated(),
		r(M::GET, "/api/admin/orgs/{uid}/grants", Operator).gated().obj(K::Org),
		r(M::POST, "/api/admin/orgs/{uid}/grants", Operator)
			.body(|_| Some(json!({ "key": "x", "amount": 1 })))
			.gated()
			.obj(K::Org),
		// plans::routes
		r(M::GET, "/api/plans/offers", Public).gated().lists(K::Offer),
		r(M::POST, "/api/plans/quote", OrgAdmin).body(quote_body).scope("plans").gated(),
		r(M::POST, "/api/plans/checkout", OrgAdmin)
			.body(|_| Some(json!({ "quoteToken": "x", "payMethod": "CARD" })))
			.scope("plans")
			.gated(),
		r(M::GET, "/api/plans/subscriptions", OrgMember)
			.scope("plans")
			.gated()
			.lists(K::Subscription),
		r(M::POST, "/api/plans/subscriptions/{uid}/cancel", OrgAdmin)
			.scope("plans")
			.gated()
			.obj(K::Subscription),
		r(M::POST, "/api/plans/subscriptions/{uid}/resume", OrgAdmin)
			.scope("plans")
			.gated()
			.obj(K::Subscription),
		r(M::POST, "/api/plans/subscriptions/{uid}/cancel-change", OrgAdmin)
			.scope("plans")
			.gated()
			.obj(K::Subscription),
		r(M::GET, "/api/admin/subscriptions", Operator).gated().lists(K::Subscription),
		// Step-up only with `immediate`; the default body carries none.
		r(M::POST, "/api/admin/subscriptions/{uid}/cancel", Operator)
			.body(|_| Some(json!({ "immediate": false })))
			.gated()
			.obj(K::Subscription),
		r(M::POST, "/api/admin/offers/{code}/reprice", Operator)
			.body(|_| Some(json!({ "currency": "HUF", "amount": "1000.00" })))
			.stepup()
			.gated()
			.obj(K::Offer),
	];
	#[cfg(feature = "ai")]
	v.extend([
		r(M::GET, "/api/agent/runs/{uid}/events", OrgMember)
			.scope("agent")
			.gated()
			.obj(K::Run),
		r(M::POST, "/api/agent/runs/{uid}/cancel", OrgMember)
			.scope("agent")
			.gated()
			.obj(K::Run),
	]);
	v
}

// vim: ts=4
