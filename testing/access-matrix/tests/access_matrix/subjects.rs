// SPDX-License-Identifier: MPL-2.0
//! The subjects: who calls. Every legitimate credential comes from a real endpoint (login,
//! switch-org, api-keys); only the shapes no route mints are forged. Liveness is killed by a
//! store write *after* minting, so the matrix proves the per-request re-read.

use std::collections::BTreeMap;

use axum::http::{Method, StatusCode};
use mintworks_auth::store::{
	Account, AccountStatus, AuthStore, LegalKind, NewConsent, NewTotpCredential, OrgKind, OrgStatus,
};
use mintworks_core::auth_mw::{Claims, JWT_SECRET_KEY};
use mintworks_core::prelude::*;
use mintworks_core::store::Role;
use serde_json::{Value, json};

use crate::fixture::{Fixture, call, forge, on_rt, req};
use crate::objects::{OrgTag, PASSWORD, account};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cred {
	/// No `Authorization` header.
	None,
	/// Not a JWT at all.
	Garbage,
	/// Correctly signed, `exp` an hour in the past.
	Expired,
	/// A well-formed token signed with a key that is not the app's.
	WrongKey,
	/// An API key revoked after minting.
	Revoked,
	Valid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
	Session,
	/// `imp` set, `auth_at` unset: no route mints it today.
	Impersonation,
	/// `<prefix>:read|write` entries as stored on the key.
	Key {
		scopes: Vec<String>,
	},
}

/// Every policy-relevant fact as plain data: the oracle reads this, never production code.
#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct SubjectFacts {
	pub cred: Cred,
	pub kind: Kind,
	/// The token's `org` claim (a key's org); `None` for a personal org or no credential.
	pub acting_org: Option<OrgTag>,
	/// Effective role per org, ancestors included, **as minted**: a later removal or
	/// suspension shows in the liveness flags below, not here.
	pub roles: BTreeMap<OrgTag, Role>,
	pub account_live: bool,
	pub org_live: bool,
	pub member_live: bool,
	/// TOS + PRIVACY accepted.
	pub consented: bool,
	/// `auth_at` within `auth.stepup_window`. Never on a key or an impersonation token.
	pub auth_fresh: bool,
	/// Accepted direct memberships, as minted: `/api/orgs` and `owner_of` read these, not `roles`.
	pub direct: BTreeMap<OrgTag, Role>,
	/// Un-accepted memberships, which `/api/orgs` lists too.
	pub pending: Vec<OrgTag>,
	/// No `org` claim, so the server acts on the account's own PERSONAL org.
	pub personal: bool,
}

#[derive(Clone, Debug)]
pub struct Subject {
	pub name: &'static str,
	pub bearer: Option<String>,
	pub facts: SubjectFacts,
	pub account_uid: Option<String>,
	/// Accepted direct memberships, what [`clone_subject`] replicates.
	pub memberships: Vec<(OrgTag, Role)>,
}

fn parent(tag: OrgTag) -> Option<OrgTag> {
	match tag {
		OrgTag::Root => None,
		OrgTag::A1 => Some(OrgTag::A),
		OrgTag::A | OrgTag::B | OrgTag::S => Some(OrgTag::Root),
	}
}

/// The ancestor walk in plain data: the best direct role on the org or any ancestor.
fn effective(direct: &[(OrgTag, Role)]) -> BTreeMap<OrgTag, Role> {
	let mut out = BTreeMap::new();
	for tag in [OrgTag::Root, OrgTag::A, OrgTag::A1, OrgTag::B, OrgTag::S] {
		let mut at = Some(tag);
		while let Some(t) = at {
			for &(o, r) in direct {
				if o == t && out.get(&tag).is_none_or(|&cur| r > cur) {
					out.insert(tag, r);
				}
			}
			at = parent(t);
		}
	}
	out
}

fn live(kind: Kind, acting: Option<OrgTag>, direct: &[(OrgTag, Role)]) -> SubjectFacts {
	let fresh = kind == Kind::Session;
	SubjectFacts {
		cred: Cred::Valid,
		kind,
		acting_org: acting,
		roles: effective(direct),
		account_live: true,
		org_live: true,
		member_live: true,
		consented: true,
		auth_fresh: fresh,
		direct: direct.iter().copied().collect(),
		pending: vec![],
		personal: false,
	}
}

async fn member(fx: &Fixture, tag: OrgTag, acct: &Account, role: Role) {
	let org = fx.orgs.of(tag).id;
	assert!(fx.store.put_membership(org, acct.id, role).await.unwrap());
	fx.store.accept_membership(org, acct.id, Timestamp::now()).await.unwrap();
}

async fn consent(fx: &Fixture, acct: &Account) {
	record_consent(fx, acct, true).await;
}

/// A newer row per gating kind; `granted: false` leaves the account owing consent.
async fn record_consent(fx: &Fixture, acct: &Account, granted: bool) {
	for kind in [LegalKind::Tos, LegalKind::Privacy] {
		let doc = fx
			.store
			.current_legal_doc(kind, &acct.locale, Timestamp::now())
			.await
			.unwrap()
			.unwrap();
		fx.store
			.record_consent(
				&NewConsent {
					account_id: acct.id,
					org_id: None,
					kind,
					legal_doc_id: Some(doc.id),
					doc_version: doc.version,
					doc_sha256: doc.sha256,
					granted,
					ip: None,
					user_agent: None,
				},
				Timestamp::now(),
			)
			.await
			.unwrap();
	}
}

/// A fresh consented account holding `direct` (accepted).
async fn person(fx: &Fixture, name: &str, direct: &[(OrgTag, Role)], consented: bool) -> Account {
	let acct = account(&fx.store, &fx.pwd_hash, &format!("{name}@matrix.invalid")).await;
	for &(tag, role) in direct {
		member(fx, tag, &acct, role).await;
	}
	if consented {
		consent(fx, &acct).await;
	}
	acct
}

async fn post(fx: &Fixture, uri: &str, bearer: Option<&str>, body: Value) -> Value {
	let r = call(&fx.router, req(Method::POST, uri, bearer, Some(body))).await;
	assert!(r.status.is_success(), "{uri}: {} {:?}", r.status, r.body);
	r.body.unwrap()
}

/// `POST /api/auth/login`, then `switch-org` unless login already landed on `to`.
async fn session(fx: &Fixture, acct: &Account, to: Option<OrgTag>) -> String {
	let body =
		post(fx, "/api/auth/login", None, json!({ "email": acct.email, "password": PASSWORD }))
			.await;
	let token = body["accessToken"].as_str().unwrap().to_owned();
	let Some(to) = to else { return token };
	let uid = &fx.orgs.of(to).uid;
	if body["org"]["uid"].as_str() == Some(uid) {
		return token;
	}
	switch(fx, &token, to).await
}

async fn switch(fx: &Fixture, token: &str, to: OrgTag) -> String {
	let body =
		post(fx, "/api/auth/switch-org", Some(token), json!({ "orgUid": fx.orgs.of(to).uid }))
			.await;
	body["accessToken"].as_str().unwrap().to_owned()
}

/// `POST /api/api-keys` under `token`'s org; `token` must be within the step-up window.
async fn mint_key(fx: &Fixture, token: &str, scopes: &[String]) -> (String, String) {
	let body =
		post(fx, "/api/api-keys", Some(token), json!({ "name": "matrix", "scopes": scopes })).await;
	(body["uid"].as_str().unwrap().to_owned(), body["key"].as_str().unwrap().to_owned())
}

async fn claims_of(fx: &Fixture, token: &str) -> Claims {
	let key = fx.app.secrets.get_or_create(JWT_SECRET_KEY, 32).await.unwrap();
	jsonwebtoken::decode::<Claims>(
		token,
		&jsonwebtoken::DecodingKey::from_secret(&key),
		&jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256),
	)
	.unwrap()
	.claims
}

fn subject(
	name: &'static str,
	bearer: Option<String>,
	facts: SubjectFacts,
	acct: Option<&Account>,
	memberships: &[(OrgTag, Role)],
) -> Subject {
	Subject {
		name,
		bearer,
		facts,
		account_uid: acct.map(|a| a.uid.as_str().to_owned()),
		memberships: memberships.to_vec(),
	}
}

/// The roster. Runs inside `build()`, on the fixture's runtime.
pub async fn roster(fx: &Fixture) -> Vec<Subject> {
	use OrgTag::{A, A1, B, Root, S};
	use Role::{Admin, Member, Owner};
	let now = Timestamp::now().0;
	let mut out = Vec::new();
	let dead = SubjectFacts {
		cred: Cred::None,
		kind: Kind::Session,
		acting_org: None,
		roles: BTreeMap::new(),
		account_live: false,
		org_live: false,
		member_live: false,
		consented: false,
		auth_fresh: false,
		direct: BTreeMap::new(),
		pending: vec![],
		personal: false,
	};
	out.push(subject("anon", None, dead.clone(), None, &[]));
	out.push(subject(
		"garbage",
		Some("not.a.jwt".into()),
		SubjectFacts { cred: Cred::Garbage, ..dead.clone() },
		None,
		&[],
	));

	// Org owners exist since `topology`; they only need consent.
	let owner = |t: OrgTag| fx.orgs.of(t).owner.clone().unwrap();
	let (owner_a, owner_b) = (owner(A), owner(B));
	for t in [A, A1, B, S] {
		consent(fx, &owner(t)).await;
	}

	// Keys are minted right after their account's login: `POST /api/api-keys` wants step-up.
	// Each account logs in once: every login charges the `login.email` bucket.
	let m_oa = [(A, Owner)];
	let tok_oa = session(fx, &owner_a, Some(A)).await;
	let claims_oa = claims_of(fx, &tok_oa).await;
	let r = call(&fx.router, req(Method::GET, "/api/api-keys/scopes", Some(&tok_oa), None)).await;
	assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
	let scopes: Vec<String> = serde_json::from_value(r.body.unwrap()["prefixes"].clone()).unwrap();
	out.push(subject(
		"owner_a",
		Some(tok_oa),
		live(Kind::Session, Some(A), &m_oa),
		Some(&owner_a),
		&m_oa,
	));
	let all: Vec<String> = scopes
		.iter()
		.flat_map(|p| [format!("{p}:read"), format!("{p}:write")])
		.collect();
	let read: Vec<String> = scopes.iter().map(|p| format!("{p}:read")).collect();

	let m_op = [(Root, Admin)];
	let operator = person(fx, "operator", &m_op, true).await;
	let op_tok = session(fx, &operator, Some(A)).await;
	let (_, key_op) = mint_key(fx, &op_tok, &all).await;
	out.push(subject(
		"operator",
		Some(op_tok),
		live(Kind::Session, Some(A), &m_op),
		Some(&operator),
		&m_op,
	));
	out.push(subject(
		"key_operator",
		Some(key_op),
		live(Kind::Key { scopes: all.clone() }, Some(A), &m_op),
		Some(&operator),
		&m_op,
	));

	let m = [(Root, Member)];
	let acct = person(fx, "root_member", &m, true).await;
	let tok = session(fx, &acct, Some(A)).await;
	out.push(subject("root_member", Some(tok), live(Kind::Session, Some(A), &m), Some(&acct), &m));

	let m_aa = [(A, Admin)];
	let admin_a = person(fx, "admin_a", &m_aa, true).await;
	let tok = session(fx, &admin_a, Some(A)).await;
	let (_, k_all) = mint_key(fx, &tok, &all).await;
	let (_, k_read) = mint_key(fx, &tok, &read).await;
	// The endpoint refuses an empty scope set; the shape is reached by emptying a minted key.
	let (none_uid, k_none) = mint_key(fx, &tok, &read[..1]).await;
	sqlx::query("UPDATE api_keys SET scopes = '[]' WHERE uid = ?")
		.bind(&none_uid)
		.execute(fx.store.write_pool())
		.await
		.unwrap();
	let (revoked_uid, k_revoked) = mint_key(fx, &tok, &all).await;
	let tok_on_a1 = switch(fx, &tok, A1).await;
	out.push(subject(
		"admin_a",
		Some(tok),
		live(Kind::Session, Some(A), &m_aa),
		Some(&admin_a),
		&m_aa,
	));
	out.push(subject(
		"admin_a_on_a1",
		Some(tok_on_a1),
		live(Kind::Session, Some(A1), &m_aa),
		Some(&admin_a),
		&m_aa,
	));
	let key = |scopes: &[String]| live(Kind::Key { scopes: scopes.to_vec() }, Some(A), &m_aa);
	out.push(subject("key_a_all", Some(k_all), key(&all), Some(&admin_a), &m_aa));
	out.push(subject("key_a_read", Some(k_read), key(&read), Some(&admin_a), &m_aa));
	out.push(subject("key_a_none", Some(k_none), key(&[]), Some(&admin_a), &m_aa));
	out.push(subject(
		"key_revoked",
		Some(k_revoked),
		SubjectFacts { cred: Cred::Revoked, ..key(&all) },
		Some(&admin_a),
		&m_aa,
	));

	// Minting is consent-gated, so consent is withdrawn only after the key exists.
	let m_ka = [(A, Admin)];
	let key_owner = person(fx, "key_unconsented", &m_ka, true).await;
	let tok = session(fx, &key_owner, Some(A)).await;
	let (_, k_unconsented) = mint_key(fx, &tok, &all).await;
	record_consent(fx, &key_owner, false).await;
	out.push(subject(
		"key_unconsented",
		Some(k_unconsented),
		SubjectFacts {
			consented: false,
			..live(Kind::Key { scopes: all.clone() }, Some(A), &m_ka)
		},
		Some(&key_owner),
		&m_ka,
	));

	let m_ma = [(A, Member)];
	let member_a = person(fx, "member_a", &m_ma, true).await;
	let tok = session(fx, &member_a, Some(A)).await;
	out.push(subject(
		"member_a",
		Some(tok),
		live(Kind::Session, Some(A), &m_ma),
		Some(&member_a),
		&m_ma,
	));

	for (name, role) in [("admin_a1", Admin), ("member_a1", Member)] {
		let m = [(A1, role)];
		let acct = person(fx, name, &m, true).await;
		let tok = session(fx, &acct, Some(A1)).await;
		out.push(subject(name, Some(tok), live(Kind::Session, Some(A1), &m), Some(&acct), &m));
	}

	let m_ob = [(B, Owner)];
	let tok = session(fx, &owner_b, Some(B)).await;
	out.push(subject(
		"owner_b",
		Some(tok),
		live(Kind::Session, Some(B), &m_ob),
		Some(&owner_b),
		&m_ob,
	));

	// Never accepted: login lands on the personal org, and `switch-org` to A must refuse.
	let invited = person(fx, "invited_a", &[], true).await;
	assert!(fx.store.put_membership(fx.orgs.a.id, invited.id, Member).await.unwrap());
	let tok = session(fx, &invited, None).await;
	let facts = SubjectFacts { pending: vec![A], personal: true, ..live(Kind::Session, None, &[]) };
	out.push(subject("invited_a", Some(tok), facts, Some(&invited), &[]));

	let removed = person(fx, "removed_a", &m_ma, true).await;
	let tok_removed = session(fx, &removed, Some(A)).await;
	let suspended = person(fx, "suspended_acct", &m_ma, true).await;
	let tok_suspended = session(fx, &suspended, Some(A)).await;
	let m_s = [(S, Member)];
	let s_member = person(fx, "suspended_org_member", &m_s, true).await;
	let tok_s = session(fx, &s_member, Some(S)).await;

	// Consent is a gate on `switch-org`, so this one relies on landing on A at login.
	let no_consent = person(fx, "no_consent", &m_ma, false).await;
	let tok = session(fx, &no_consent, None).await;
	assert_eq!(claims_of(fx, &tok).await.org.as_deref(), Some(fx.orgs.a.uid.as_str()));
	out.push(subject(
		"no_consent",
		Some(tok),
		SubjectFacts { consented: false, ..live(Kind::Session, Some(A), &m_ma) },
		Some(&no_consent),
		&m_ma,
	));

	// Forged shapes, all on owner_a's (or member_a's) real claims.
	let stale = Claims { auth_at: Some(now - 3_600), ..claims_oa.clone() };
	out.push(subject(
		"owner_a_stale",
		Some(forge(&fx.app, &stale).await),
		SubjectFacts { auth_fresh: false, ..live(Kind::Session, Some(A), &m_oa) },
		Some(&owner_a),
		&m_oa,
	));
	let expired = Claims { iat: now - 4_500, exp: now - 3_600, ..claims_oa.clone() };
	out.push(subject(
		"expired",
		Some(forge(&fx.app, &expired).await),
		SubjectFacts { cred: Cred::Expired, ..live(Kind::Session, Some(A), &m_oa) },
		Some(&owner_a),
		&m_oa,
	));
	let wrong = jsonwebtoken::encode(
		&jsonwebtoken::Header::default(),
		&claims_oa,
		&jsonwebtoken::EncodingKey::from_secret(b"not-the-app-key-not-the-app-key!"),
	)
	.unwrap();
	out.push(subject(
		"wrong_key",
		Some(wrong),
		SubjectFacts { cred: Cred::WrongKey, ..live(Kind::Session, Some(A), &m_oa) },
		Some(&owner_a),
		&m_oa,
	));
	let imp = Claims {
		sub: member_a.uid.as_str().to_owned(),
		org: Some(fx.orgs.a.uid.clone()),
		rol: Some(Member.as_str().to_owned()),
		opr: false,
		ep: member_a.token_epoch,
		auth_at: None,
		ses: None,
		imp: Some(operator.uid.as_str().to_owned()),
		typ: None,
		iat: now,
		exp: now + 900,
	};
	out.push(subject(
		"impersonation",
		Some(forge(&fx.app, &imp).await),
		live(Kind::Impersonation, Some(A), &m_ma),
		Some(&member_a),
		&m_ma,
	));

	// Liveness, killed after minting.
	assert!(fx.store.remove_membership(fx.orgs.a.id, removed.id).await.unwrap());
	out.push(subject(
		"removed_a",
		Some(tok_removed),
		SubjectFacts { member_live: false, ..live(Kind::Session, Some(A), &m_ma) },
		Some(&removed),
		&m_ma,
	));
	// Suspension also bumps `token_epoch`, so this token dies as "superseded" first.
	fx.store
		.set_account_status(suspended.id, AccountStatus::Suspended)
		.await
		.unwrap();
	out.push(subject(
		"suspended_acct",
		Some(tok_suspended),
		SubjectFacts { account_live: false, ..live(Kind::Session, Some(A), &m_ma) },
		Some(&suspended),
		&m_ma,
	));
	fx.store
		.update_org(fx.orgs.s.id, None, Patch::Undefined, Some(OrgStatus::Suspended))
		.await
		.unwrap();
	out.push(subject(
		"suspended_org_member",
		Some(tok_s),
		SubjectFacts { org_live: false, ..live(Kind::Session, Some(S), &m_s) },
		Some(&s_member),
		&m_s,
	));
	let revoked = ApiKeyId::parse(&revoked_uid).unwrap();
	assert!(fx.store.revoke_api_key(fx.orgs.a.id, &revoked, Timestamp::now()).await.unwrap());
	out
}

/// A disposable twin of a live session subject, for routes that mutate the caller itself: a
/// fresh account with the same memberships and consent, its token forged in the claim shape
/// the real endpoint produced for the original.
pub async fn clone_subject(fx: &'static Fixture, s: &Subject) -> Subject {
	let s = s.clone();
	on_rt(async move {
		assert!(
			s.facts.cred == Cred::Valid
				&& s.facts.kind != Kind::Impersonation
				&& !matches!(s.facts.kind, Kind::Key { .. }),
			"{} is not a live session subject",
			s.name
		);
		let n = CLONES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
		let name: &'static str = Box::leak(format!("{}_clone{n}", s.name).into_boxed_str());
		let acct = person(fx, name, &s.memberships, s.facts.consented).await;
		let acct = fx.store.account_by_id(acct.id).await.unwrap().unwrap();
		let original = claims_of(fx, s.bearer.as_deref().unwrap()).await;
		// The original's PERSONAL org is not the clone's: name the clone's own, or the claim is dead.
		let org = if s.facts.personal {
			mintworks_auth::store::personal_org(&*fx.store, &acct.uid).await.unwrap()
		} else {
			original.org.clone()
		};
		let now = Timestamp::now().0;
		let claims = Claims {
			sub: acct.uid.as_str().to_owned(),
			opr: acct.is_root_admin,
			ep: acct.token_epoch,
			// Keeps the original's age, so a stale subject's clone is stale too.
			auth_at: original.auth_at.map(|a| now - (original.iat - a)),
			iat: now,
			exp: now + (original.exp - original.iat),
			org,
			..original
		};
		Subject {
			name,
			bearer: Some(forge(&fx.app, &claims).await),
			account_uid: Some(acct.uid.as_str().to_owned()),
			..s
		}
	})
	.await
}

/// The account behind a subject, re-read so `token_epoch` is current.
pub async fn account_of(fx: &'static Fixture, s: &Subject) -> Account {
	let uid = AccountId::parse(s.account_uid.as_deref().unwrap()).unwrap();
	on_rt(async move { fx.store.account_by_uid(&uid).await.unwrap().unwrap() }).await
}

fn unique(prefix: &str) -> String {
	let n = CLONES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
	format!("{prefix}{n}")
}

fn with_bearer(base: &Subject, name: &'static str, bearer: String) -> Subject {
	Subject { name, bearer: Some(bearer), ..base.clone() }
}

/// The credentials curated rows name that the roster does not hold. Each call builds a new one.
pub async fn special(fx: &'static Fixture, name: &'static str) -> Subject {
	on_rt(special_on_rt(fx, name)).await
}

async fn special_on_rt(fx: &'static Fixture, name: &'static str) -> Subject {
	let base = |n: &str| fx.subject(n).clone();
	let claims = async |n: &str| claims_of(fx, fx.subject(n).bearer.as_deref().unwrap()).await;
	let now = Timestamp::now().0;
	let token = match name {
		"fresh(no_consent)" => return clone_subject(fx, fx.subject("no_consent")).await,
		"fresh(removed_a)" => {
			let s = clone_subject(fx, fx.subject("member_a")).await;
			let acct = account_of(fx, &s).await;
			assert!(fx.store.remove_membership(fx.orgs.a.id, acct.id).await.unwrap());
			return Subject { name, ..s };
		}
		"fresh(impersonation)" => {
			let s = clone_subject(fx, fx.subject("member_a")).await;
			let c = claims_of(fx, s.bearer.as_deref().unwrap()).await;
			let imp = fx.subject("operator").account_uid.clone();
			let c = Claims { imp, auth_at: None, ..c };
			return with_bearer(&s, name, forge(&fx.app, &c).await);
		}
		// A new SHARED org under ROOT, suspended after the token is minted.
		"fresh(suspended_org_member)" => {
			let tag = unique("susp");
			let owner =
				account(&fx.store, &fx.pwd_hash, &format!("{tag}-owner@matrix.invalid")).await;
			let org = fx.store.create_org(OrgKind::Shared, fx.orgs.root.id, &tag, owner.id, None);
			let org = org.await.unwrap();
			let acct = account(&fx.store, &fx.pwd_hash, &format!("{tag}@matrix.invalid")).await;
			assert!(fx.store.put_membership(org.id, acct.id, Role::Member).await.unwrap());
			fx.store.accept_membership(org.id, acct.id, Timestamp::now()).await.unwrap();
			consent(fx, &acct).await;
			let c = Claims {
				sub: acct.uid.as_str().to_owned(),
				org: Some(org.uid.as_str().to_owned()),
				ep: acct.token_epoch,
				auth_at: Some(now),
				iat: now,
				exp: now + 900,
				..claims("suspended_org_member").await
			};
			let tok = forge(&fx.app, &c).await;
			fx.store
				.update_org(org.id, None, Patch::Undefined, Some(OrgStatus::Suspended))
				.await
				.unwrap();
			let s = base("suspended_org_member");
			return Subject {
				account_uid: Some(acct.uid.as_str().to_owned()),
				..with_bearer(&s, name, tok)
			};
		}
		"totp_pending" => {
			let acct = person(fx, &unique("totp"), &[(OrgTag::A, Role::Member)], true).await;
			let totp = NewTotpCredential {
				account_id: acct.id,
				secret_nonce: vec![0; 12],
				secret_enc: vec![0; 32],
				digits: 6,
				period: 30,
				recovery_hashes: "[]".into(),
			};
			assert!(fx.store.put_totp(&totp).await.unwrap());
			assert!(fx.store.confirm_totp(acct.id, Timestamp::now(), "[]").await.unwrap());
			let body = json!({ "email": acct.email, "password": PASSWORD });
			let r = call(&fx.router, req(Method::POST, "/api/auth/login", None, Some(body))).await;
			let b = r.body.unwrap_or_default();
			b["totpToken"]
				.as_str()
				.unwrap_or_else(|| panic!("no totpToken: {b}"))
				.to_owned()
		}
		// member_a's own refresh token would need a second member_a login; an account in the
		// same state is equivalent.
		"refresh_tok" => {
			let acct = person(fx, &unique("refresh"), &[(OrgTag::A, Role::Member)], true).await;
			let body = json!({ "email": acct.email, "password": PASSWORD });
			let b = post(fx, "/api/auth/login", None, body).await;
			b["refreshToken"].as_str().unwrap().to_owned()
		}
		"no_org_tok" => forge(&fx.app, &Claims { org: None, ..claims("member_a").await }).await,
		"foreign_org_tok" => {
			let org = Some(fx.orgs.b.uid.clone());
			forge(&fx.app, &Claims { org, ..claims("member_a").await }).await
		}
		"ep_tok" => {
			let c = claims("member_a").await;
			forge(&fx.app, &Claims { ep: c.ep + 1, ..c }).await
		}
		"ghost_tok" => {
			let sub = AccountId::generate().into_string();
			forge(&fx.app, &Claims { sub, ..claims("member_a").await }).await
		}
		"susp_live_tok" => {
			let ep = account_of(fx, fx.subject("suspended_acct")).await.token_epoch;
			forge(&fx.app, &Claims { ep, ..claims("suspended_acct").await }).await
		}
		"no_authat_tok" => {
			let c = Claims { auth_at: None, imp: None, ..claims("owner_a").await };
			forge(&fx.app, &c).await
		}
		_ => panic!("no builder for subject {name}"),
	};
	let from = match name {
		"susp_live_tok" => "suspended_acct",
		"no_authat_tok" => "owner_a",
		_ => "member_a",
	};
	with_bearer(&base(from), name, token)
}

static CLONES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

// vim: ts=4
