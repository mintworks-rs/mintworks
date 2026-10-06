// SPDX-License-Identifier: MPL-2.0
//! The independent oracle: the *intended* policy of `api-surface.md` §1.8 over plain facts.
//! It must never call production code — a suspected bug has to surface as a mismatch, so the
//! oracle encodes what should happen, not what does.

use axum::http::Method;
use mintworks_core::store::Role;

use crate::objects::{Obj, ObjKind, OrgTag};
use crate::routes::{Level, RouteSpec};
use crate::subjects::{Cred, Kind, SubjectFacts};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Outcome {
	Allow,
	Deny(&'static str),
}

#[derive(Clone, Copy, Debug)]
pub struct Expect {
	pub outcome: Outcome,
	/// The rung that decided: the report's grouping key.
	pub rule: &'static str,
	/// List routes: whether the object should appear in the listing.
	pub listed: Option<bool>,
}

type Rung = fn(&SubjectFacts, &RouteSpec, Option<&Obj>) -> Option<&'static str>;

/// Ordered: level before step-up, so step-up never reveals a route to a non-member.
const RUNGS: &[(&str, Rung)] = &[
	("cred", cred),
	("key.live", key_live),
	("account", account),
	("key.scope", key_scope),
	("consent", consent),
	("confine", confine),
	("level", level),
	("stepup", stepup),
	("object", object),
];

pub fn expected(s: &SubjectFacts, r: &RouteSpec, o: Option<&Obj>) -> Expect {
	// The refresh credential is the body's or cookie's refresh token, which the matrix never sends.
	if r.path == "/api/auth/refresh" {
		return Expect { outcome: Outcome::Deny("E-AUTH-TOKEN"), rule: "public", listed: None };
	}
	if r.level == Level::Public {
		return Expect { outcome: Outcome::Allow, rule: "public", listed: None };
	}
	for &(rule, rung) in RUNGS {
		if let Some(code) = rung(s, r, o) {
			return Expect { outcome: Outcome::Deny(code), rule, listed: None };
		}
	}
	// An operator listing is platform-wide; `/api/orgs` lists direct memberships, pending too.
	let listed = r.lists.and(o).map(|o| match r.lists {
		_ if r.level == Level::Operator => true,
		Some(ObjKind::Org) => direct(s, o.org).is_some() || s.pending.contains(&o.org),
		_ => sees(s, r, o.org),
	});
	Expect { outcome: Outcome::Allow, rule: "allow", listed }
}

/// Whether `org`'s objects are in reach of the route's home org.
fn sees(s: &SubjectFacts, r: &RouteSpec, org: OrgTag) -> bool {
	let home = home(s, r);
	// An API key is its creator's or an admin's; no non-admin roster subject created one.
	if r.object == Some(ObjKind::ApiKey) || r.lists == Some(ObjKind::ApiKey) {
		return home == Some(org) && admin(role(s, org));
	}
	// accepted: a child org reads its parent's service catalogue.
	let catalogue = r.method == Method::GET
		&& (r.object == Some(ObjKind::Service) || r.lists == Some(ObjKind::Service));
	home == Some(org) || (catalogue && home == Some(OrgTag::A1) && org == OrgTag::A)
}

fn is_key(s: &SubjectFacts) -> bool {
	matches!(s.kind, Kind::Key { .. })
}

/// The acting org as the server resolves it: a dead membership or org reads as none.
fn acting(s: &SubjectFacts) -> Option<OrgTag> {
	s.acting_org.filter(|_| s.org_live && s.member_live)
}

/// Effective role, ancestors included; nothing once the acting membership or org is dead.
// Liveness is tracked per subject, not per org; per-org flags if a subject ever spans two.
fn role(s: &SubjectFacts, org: OrgTag) -> Option<Role> {
	(s.org_live && s.member_live).then(|| s.roles.get(&org).copied()).flatten()
}

/// A direct accepted membership, with no ancestor walk: what `owner_of` reads.
fn direct(s: &SubjectFacts, org: OrgTag) -> Option<Role> {
	(s.org_live && s.member_live).then(|| s.direct.get(&org).copied()).flatten()
}

/// The org owning the seller the acting org files under.
fn seller_org(org: Option<OrgTag>) -> Option<OrgTag> {
	match org? {
		OrgTag::A | OrgTag::A1 => Some(OrgTag::A),
		OrgTag::B => Some(OrgTag::B),
		OrgTag::Root | OrgTag::S => None,
	}
}

/// The org a route's objects must belong to.
fn home(s: &SubjectFacts, r: &RouteSpec) -> Option<OrgTag> {
	match r.level {
		Level::SellerAdmin => seller_org(acting(s)),
		_ => acting(s),
	}
}

fn admin(role: Option<Role>) -> bool {
	role.is_some_and(|r| r >= Role::Admin)
}

fn cred(s: &SubjectFacts, _: &RouteSpec, _: Option<&Obj>) -> Option<&'static str> {
	// No subject carries a superseded `ep`: the roster has no deliberately stale-epoch token.
	matches!(s.cred, Cred::None | Cred::Garbage | Cred::Expired | Cred::WrongKey)
		.then_some("E-AUTH-TOKEN")
}

fn key_live(s: &SubjectFacts, _: &RouteSpec, _: Option<&Obj>) -> Option<&'static str> {
	let dead = !(s.account_live && s.org_live && s.member_live);
	(s.cred == Cred::Revoked || (is_key(s) && dead)).then_some("E-AUTH-KEY-REVOKED")
}

/// Suspension bumps `token_epoch`, so a token minted before it is superseded before the status
/// check that would answer `E-AUTH-SUSPENDED`; and a suspended account cannot mint a new one.
fn account(s: &SubjectFacts, _: &RouteSpec, _: Option<&Obj>) -> Option<&'static str> {
	(!is_key(s) && !s.account_live).then_some("E-AUTH-TOKEN")
}

fn key_scope(s: &SubjectFacts, r: &RouteSpec, _: Option<&Obj>) -> Option<&'static str> {
	let Kind::Key { scopes } = &s.kind else { return None };
	let Some(prefix) = r.scope else { return Some("E-AUTH-SCOPE") };
	let read = r.method == Method::GET || r.method == Method::HEAD;
	let need = format!("{prefix}:{}", if read { "read" } else { "write" });
	(!scopes.contains(&need)).then_some("E-AUTH-SCOPE")
}

/// Keys skip it: consent binds the account holder's session, not a scoped key.
fn consent(s: &SubjectFacts, r: &RouteSpec, _: Option<&Obj>) -> Option<&'static str> {
	(!is_key(s) && r.consent_gated && !s.consented).then_some("E-AUTH-CONSENT-REQUIRED")
}

/// An invoice is looked up in the acting org before any role check: another org's invoice is
/// NOTFOUND, even to an admin of its seller's org acting from a child.
fn confine(s: &SubjectFacts, r: &RouteSpec, o: Option<&Obj>) -> Option<&'static str> {
	let o = o.filter(|_| r.object.is_some() && r.level == Level::SellerAdmin)?;
	let invoice = matches!(o.kind, ObjKind::DraftInvoice | ObjKind::IssuedInvoice);
	(invoice && acting(s).is_some_and(|a| a != o.org)).then_some("E-CORE-NOTFOUND")
}

fn level(s: &SubjectFacts, r: &RouteSpec, o: Option<&Obj>) -> Option<&'static str> {
	const FORBIDDEN: Option<&str> = Some("E-AUTH-FORBIDDEN");
	match r.level {
		Level::Public | Level::Authenticated => None,
		// The PERSONAL org has no seller.
		_ if s.personal && r.level == Level::SellerAdmin => Some("E-CORE-NOTFOUND"),
		Level::OrgMember | Level::OrgAdmin
			if s.personal
				&& ((r.method == Method::GET
					&& ["/api/seller", "/api/services"].iter().any(|p| r.path.starts_with(p)))
					// `quote` reads the buyer's own seller catalogue.
					|| r.path == "/api/plans/quote") =>
		{
			Some("E-CORE-NOTFOUND")
		}
		Level::OrgMember | Level::OrgAdmin if s.personal => None,
		Level::OrgMember => acting(s).is_none().then_some("E-AUTH-FORBIDDEN"),
		Level::OrgAdmin => match acting(s) {
			Some(org) if admin(role(s, org)) => None,
			_ => FORBIDDEN,
		},
		Level::SellerAdmin => match seller_org(acting(s)) {
			Some(org) if admin(role(s, org)) => None,
			_ => FORBIDDEN,
		},
		Level::OrgOwner => {
			// A minted org object is a fresh child whose only member is its fixture owner, which a
			// `self_mut` clone never is.
			if o.is_some_and(|o| r.object.is_some() && o.kind == ObjKind::Org) {
				return Some("E-CORE-NOTFOUND");
			}
			// The seller's org for `/api/seller/*`, else the acting org; reads only a direct
			// membership, like `owner_of`.
			if r.path.starts_with("/api/seller") {
				if s.personal {
					return Some("E-CORE-NOTFOUND");
				}
				let Some(named) = seller_org(acting(s)) else { return FORBIDDEN };
				// The caller's own seller: no role there is 403, not NOTFOUND.
				return (role(s, named) != Some(Role::Owner)).then_some("E-AUTH-FORBIDDEN");
			}
			// `transfer_body` names A whatever the acting org.
			let named =
				if r.path == "/api/org/transfer-ownership" { Some(OrgTag::A) } else { acting(s) };
			let Some(named) = named else { return FORBIDDEN };
			match direct(s, named) {
				None => Some("E-CORE-NOTFOUND"),
				Some(Role::Owner) => None,
				Some(_) => FORBIDDEN,
			}
		}
		Level::Operator => (!admin(role(s, OrgTag::Root))).then_some("E-AUTH-FORBIDDEN"),
	}
}

fn stepup(s: &SubjectFacts, r: &RouteSpec, _: Option<&Obj>) -> Option<&'static str> {
	if !r.stepup || s.auth_fresh {
		return None;
	}
	Some(if s.kind == Kind::Session { "E-AUTH-STEPUP" } else { "E-AUTH-STEPUP-IMPOSSIBLE" })
}

/// Another tenant's object is NOTFOUND, never 403. Every level but Operator is confined, and
/// OrgOwner's membership check on the named org already is its object check.
fn object(s: &SubjectFacts, r: &RouteSpec, o: Option<&Obj>) -> Option<&'static str> {
	let o = o.filter(|o| r.object.is_some() && !o.kind.global())?;
	// A bearer invite is accepted by whoever holds the code.
	if r.path == "/api/org/invites/{code}/accept" {
		return None;
	}
	if matches!(r.level, Level::Operator | Level::OrgOwner) {
		return None;
	}
	(!sees(s, r, o.org)).then_some("E-CORE-NOTFOUND")
}

// vim: ts=4
