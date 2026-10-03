//! `AuthStore` integration tests for the store-level authorization and account-safety
//! guarantees: GDPR erasure scope and irreversibility, membership revocation, the TOTP
//! compare-and-swaps, activation's first password, and API-key org scoping.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use saas_auth::store::{
	AccountStatus, AuthStore, ErasurePlan, NewAccount, NewApiKey, NewTotpCredential,
	NewWebauthnCredential, OrgKind, OrgStatus, Role,
};
use saas_core::objects::ObjectStore;
use saas_core::store::CoreStore;
use saas_core::{config::Config, prelude::*};
use serde_json::json;
use store_adapter_sqlite::SqliteStore;

/// A temp directory that takes the database with it.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir =
			std::env::temp_dir().join(format!("saas-auth-test-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}

	fn path(&self) -> String {
		self.0.join("test.db").to_string_lossy().into_owned()
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

/// The baseline brings `saas-invoice/init` along: `billing_parties` — the one cross-crate
/// entry on the erasure allowlist — lives there.
async fn setup(db: &TmpDb) -> SqliteStore {
	let store = SqliteStore::open(&Config {
		master_key: [0; 32],
		db_path: db.path(),
		data_dir: db.0.to_string_lossy().into_owned(),
		listen: String::new(),
		base_url: String::new(),
		jobs_workers: None,
	})
	.await
	.unwrap();
	store.migrate(&[store_adapter_sqlite::FRAMEWORK]).await.unwrap();
	store
}

fn new_account(email: &str) -> NewAccount {
	NewAccount {
		email: email.to_owned(),
		pwd_hash: Some("argon2-placeholder".to_owned()),
		name: None,
		locale: "hu".to_owned(),
		org_name: email.to_owned(),
	}
}

async fn add_party(store: &SqliteStore, org_id: i64, uid: &str) {
	sqlx::query(
		"INSERT INTO billing_parties (uid, org_id, kind, name, country, city, created_at,
			updated_at)
		 VALUES (?, ?, 'P', 'Kiss Anna', 'HU', 'Budapest', 0, 0)",
	)
	.bind(uid)
	.bind(org_id)
	.execute(store.write_pool())
	.await
	.unwrap();
}

async fn party_name(store: &SqliteStore, uid: &str) -> String {
	sqlx::query_scalar("SELECT name FROM billing_parties WHERE uid = ?")
		.bind(uid)
		.fetch_one(store.read_pool())
		.await
		.unwrap()
}

/// The allowlist itself is `saas_auth::gdpr::ERASURE` and is `pub(crate)` to that crate — what
/// the adapter owes is honouring *whatever* plan it is handed, and the `kind = 'PERSONAL'` scoping it
/// cannot read off the plan. So the suite brings its own, shaped like the real one.
const ERASURE: ErasurePlan = ErasurePlan {
	accounts: &[("name", None), ("pwd_hash", None)],
	orgs: &[("name", Some("[erased]"))],
	objects: &[("body", Some("{}"))],
	agent_runs: &[("spec", Some("{}")), ("error", None), ("account_id", None)],
	billing_parties: &[
		("name", Some("[erased]")),
		("postcode", None),
		("city", None),
		("street", None),
		("email", None),
	],
	delete_by_account: &["totp_credentials"],
	blank_job_kinds: &["SEND_EMAIL"],
};

/// `false`, and **nothing written**, while the account still owns an organisation. The service
/// pre-checks this on the reader pool for the message, so a `POST /api/orgs` landing between
/// that read and this transaction anonymized the owner of a live organisation: `remove_member`
/// refuses to remove an `OWNER` and `set_member_role` refuses to assign one, so no route
/// recovers it. Erasing an owner is also no licence to destroy the organisation's customer
/// records, which are other people's data.
#[tokio::test]
async fn anonymize_account_refuses_an_account_that_still_owns_an_organisation() {
	let db = TmpDb::new("erasure-scope");
	let store = setup(&db).await;

	let (account, personal) = store.create_account(&new_account("owner@e.st"), &[]).await.unwrap();
	let org = store
		.create_org(
			OrgKind::Shared,
			store.root_org_id().await.unwrap(),
			"Céges Kft.",
			account.id,
			None,
		)
		.await
		.unwrap();
	add_party(&store, personal.id, "prt_personal").await;
	add_party(&store, org.id, "prt_org").await;

	assert!(!store.anonymize_account(account.id, Timestamp(1_000), &ERASURE).await.unwrap());

	let row = store.account_by_id(account.id).await.unwrap().unwrap();
	assert_eq!(row.email, "owner@e.st", "a refused erasure writes nothing");
	assert_eq!(row.status, saas_auth::store::AccountStatus::Pending);
	assert_eq!(party_name(&store, "prt_personal").await, "Kiss Anna");
	assert_eq!(party_name(&store, "prt_org").await, "Kiss Anna");
}

/// A key belongs to the person, not to an org. Scoped to the subject's *personal* org,
/// erasure left a mere member's organisation-scoped keys live while `gdpr`'s module doc
/// promised "every key revoked".
#[tokio::test]
async fn erasure_revokes_the_subjects_organisation_keys() {
	let db = TmpDb::new("erasure-api-keys");
	let store = setup(&db).await;

	let (owner, _) = store.create_account(&new_account("owner@e.st"), &[]).await.unwrap();
	let (member, _) = store.create_account(&new_account("member@e.st"), &[]).await.unwrap();
	let org = store
		.create_org(
			OrgKind::Shared,
			store.root_org_id().await.unwrap(),
			"Céges Kft.",
			owner.id,
			None,
		)
		.await
		.unwrap();
	store.put_membership(org.id, member.id, Role::Member).await.unwrap();
	store
		.create_api_key(
			&NewApiKey {
				org_id: org.id,
				account_id: member.id,
				name: "member key".to_owned(),
				prefix: "mmmmmmmm".to_owned(),
				key_hash: "argon2-member".to_owned(),
				scopes: "[]".to_owned(),
				expires_at: None,
			},
			i64::MAX,
		)
		.await
		.unwrap();

	store.anonymize_account(member.id, Timestamp(1_000), &ERASURE).await.unwrap();

	let key = store.api_key_by_prefix("mmmmmmmm").await.unwrap().unwrap();
	assert_eq!(key.revoked_at, Some(Timestamp(1_000)));
}

/// Removal ends *this* org and nothing else. The middleware's org lookup must stop
/// resolving, and `token_epoch` must stay put: it is account-wide, so bumping it would let
/// one org's admin sign the account out of every other org it belongs to.
#[tokio::test]
async fn removing_a_membership_revokes_the_member_at_once() {
	let db = TmpDb::new("membership-revoke");
	let store = setup(&db).await;

	let (owner, _) = store.create_account(&new_account("owner@e.st"), &[]).await.unwrap();
	let (member, _) = store.create_account(&new_account("member@e.st"), &[]).await.unwrap();
	let org = store
		.create_org(
			OrgKind::Shared,
			store.root_org_id().await.unwrap(),
			"Céges Kft.",
			owner.id,
			None,
		)
		.await
		.unwrap();
	store.put_membership(org.id, member.id, Role::Member).await.unwrap();

	// The exact lookup `saas_core::auth_mw` does to turn a `org` claim into `Ctx.org_id`.
	let resolves = async |account_id: i64| -> Option<i64> {
		sqlx::query_scalar::<_, i64>(
			"SELECT t.id FROM orgs t
			 JOIN memberships m ON m.org_id = t.id
			 WHERE t.uid = ? AND m.account_id = ?",
		)
		.bind(org.uid.as_str())
		.bind(account_id)
		.fetch_optional(store.read_pool())
		.await
		.unwrap()
	};

	assert_eq!(resolves(member.id).await, Some(org.id));
	let before = store.account_by_id(member.id).await.unwrap().unwrap().token_epoch;

	assert!(store.remove_membership(org.id, member.id).await.unwrap());

	assert_eq!(resolves(member.id).await, None, "a removed member must not resolve the org");
	let after = store.account_by_id(member.id).await.unwrap().unwrap().token_epoch;
	assert_eq!(after, before, "a membership change must not sign the account out everywhere");

	// A no-op removal changes nothing either.
	assert!(!store.remove_membership(org.id, member.id).await.unwrap());
	let again = store.account_by_id(member.id).await.unwrap().unwrap().token_epoch;
	assert_eq!(again, before);

	// And the same account's *other* orgs keep their membership: it is the removed org
	// that ends, not the member.
	let other = store
		.create_org(
			OrgKind::Shared,
			store.root_org_id().await.unwrap(),
			"Masik Kft.",
			owner.id,
			None,
		)
		.await
		.unwrap();
	store.put_membership(other.id, member.id, Role::Member).await.unwrap();
	store.accept_membership(other.id, member.id, Timestamp::now()).await.unwrap();
	assert!(!store.remove_membership(org.id, member.id).await.unwrap());
	assert_eq!(
		store.accepted_membership_role(other.id, member.id).await.unwrap(),
		Some(Role::Member),
		"removing one membership must leave the account's other orgs alone"
	);
}

/// `set_member_role`, `remove_member` and `attach_member` guard the owner by reading
/// `membership_role` off the **reader pool**, where a `transfer_org_ownership` that has not
/// committed yet is invisible, and then wrote unconditionally. The loser's write then left
/// `orgs.owner_account_id` pointing at an account with no membership row, and `owner_of`
/// answers `E-CORE-NOTFOUND` for everyone — no transfer, no deletion, no erasure, ever.
#[tokio::test]
async fn the_owner_membership_survives_a_concurrent_remove() {
	let db = TmpDb::new("owner-membership-race");
	let store = setup(&db).await;

	let (owner, _) = store.create_account(&new_account("owner@e.st"), &[]).await.unwrap();
	let (member, _) = store.create_account(&new_account("member@e.st"), &[]).await.unwrap();
	let org = store
		.create_org(
			OrgKind::Shared,
			store.root_org_id().await.unwrap(),
			"Céges Kft.",
			owner.id,
			None,
		)
		.await
		.unwrap();
	store.put_membership(org.id, member.id, Role::Member).await.unwrap();
	store.accept_membership(org.id, member.id, Timestamp::now()).await.unwrap();

	assert!(store.transfer_org_ownership(org.id, owner.id, member.id).await.unwrap());

	// Both are the write the losing request would have issued after its stale read.
	assert!(!store.remove_membership(org.id, member.id).await.unwrap());
	assert!(!store.put_membership(org.id, member.id, Role::Member).await.unwrap());
	assert_eq!(
		store.accepted_membership_role(org.id, member.id).await.unwrap(),
		Some(Role::Owner),
		"the org must stay administrable"
	);
}

/// The compare-and-swap behind `totp::spend_recovery`. Both racing requests read the same
/// array; only the one that writes first may win, or a code spent by one is resurrected by
/// the other.
#[tokio::test]
async fn a_recovery_code_set_can_only_be_swapped_once() {
	let db = TmpDb::new("recovery-cas");
	let store = setup(&db).await;

	let (account, _) = store.create_account(&new_account("2fa@e.st"), &[]).await.unwrap();
	let both = r#"["hash-a","hash-b"]"#;
	store
		.put_totp(&NewTotpCredential {
			account_id: account.id,
			secret_nonce: vec![0; 12],
			secret_enc: vec![0; 32],
			digits: 6,
			period: 30,
			recovery_hashes: both.to_owned(),
		})
		.await
		.unwrap();

	// Two requests, each having read `both`, each spending one of the two codes.
	let first = store.swap_totp_recovery(account.id, both, r#"["hash-b"]"#).await.unwrap();
	let second = store.swap_totp_recovery(account.id, both, r#"["hash-a"]"#).await.unwrap();
	assert!(first, "the first swap holds the value it read and must win");
	assert!(!second, "the loser must not clobber the winner's write");

	let left: String =
		sqlx::query_scalar("SELECT recovery_hashes FROM totp_credentials WHERE account_id = ?")
			.bind(account.id)
			.fetch_one(store.read_pool())
			.await
			.unwrap();
	assert_eq!(left, r#"["hash-b"]"#);
}

/// The precondition on `put_totp`. `begin_enrolment` read `totp_by_account`, saw
/// `confirmed_at IS NULL` and *then* upserted, so a `confirm_totp` landing between the two
/// was wiped by the unconditional `DO UPDATE`: 2FA off, and the user holding eight printed
/// recovery codes that matched nothing.
#[tokio::test]
async fn an_enrolment_cannot_wipe_a_confirmed_credential() {
	let db = TmpDb::new("totp-enrol-cas");
	let store = setup(&db).await;

	let (account, _) = store.create_account(&new_account("wipe@e.st"), &[]).await.unwrap();
	let credential = |hashes: &str| NewTotpCredential {
		account_id: account.id,
		secret_nonce: vec![0; 12],
		secret_enc: vec![0; 32],
		digits: 6,
		period: 30,
		recovery_hashes: hashes.to_owned(),
	};

	// An unconfirmed row is still replaceable — a restarted enrolment is legitimate.
	assert!(store.put_totp(&credential("[]")).await.unwrap());
	assert!(store.put_totp(&credential("[]")).await.unwrap());

	assert!(store.confirm_totp(account.id, Timestamp(1_000), r#"["hash-a"]"#).await.unwrap());
	assert!(
		!store.put_totp(&credential("[]")).await.unwrap(),
		"an enrolment overwrote a confirmed credential"
	);

	let (confirmed, hashes): (Option<i64>, String) = sqlx::query_as(
		"SELECT confirmed_at, recovery_hashes FROM totp_credentials WHERE account_id = ?",
	)
	.bind(account.id)
	.fetch_one(store.read_pool())
	.await
	.unwrap();
	assert_eq!(confirmed, Some(1_000), "2FA was silently turned off");
	assert_eq!(hashes, r#"["hash-a"]"#, "the printed codes no longer match anything");
}

/// `last_used_step < ?` is the whole of the TOTP replay guard — the only thing stopping a
/// shoulder-surfed, logged or proxied 6-digit code being spent twice — and nothing presented
/// the same code twice anywhere. An adapter writing `<=` passes the rest of this suite and
/// ships a second-factor bypass.
#[tokio::test]
async fn a_totp_code_cannot_be_spent_twice() {
	let db = TmpDb::new("totp-replay");
	let store = setup(&db).await;

	let (account, _) = store.create_account(&new_account("replay@e.st"), &[]).await.unwrap();
	store
		.put_totp(&NewTotpCredential {
			account_id: account.id,
			secret_nonce: vec![0; 12],
			secret_enc: vec![0; 32],
			digits: 6,
			period: 30,
			recovery_hashes: "[]".to_owned(),
		})
		.await
		.unwrap();

	assert!(store.advance_totp_step(account.id, 100).await.unwrap());
	assert!(!store.advance_totp_step(account.id, 100).await.unwrap(), "a code was replayed");
	assert!(!store.advance_totp_step(account.id, 99).await.unwrap(), "the counter rewound");
	assert!(store.advance_totp_step(account.id, 101).await.unwrap(), "the next step is legal");
}

/// An invited account is created with `pwd_hash = NULL`; without this it activated into an
/// `ACTIVE` account that could never log in.
#[tokio::test]
async fn activation_sets_an_invited_accounts_first_password() {
	let db = TmpDb::new("activate-password");
	let store = setup(&db).await;

	let invited = NewAccount { pwd_hash: None, ..new_account("invited@e.st") };
	let (account, _) = store.create_account(&invited, &[]).await.unwrap();
	assert!(account.pwd_hash.is_none());

	assert!(
		store
			.activate_account(account.id, Some("argon2-set-here"), Timestamp(1_000))
			.await
			.unwrap()
	);
	let fresh = store.account_by_id(account.id).await.unwrap().unwrap();
	assert_eq!(fresh.pwd_hash.as_deref(), Some("argon2-set-here"));
	assert_eq!(fresh.activated_at, Some(Timestamp(1_000)));

	// Replay: no longer PENDING, so nothing is touched — least of all the password.
	assert!(
		!store
			.activate_account(account.id, Some("other"), Timestamp(2_000))
			.await
			.unwrap()
	);
	let again = store.account_by_id(account.id).await.unwrap().unwrap();
	assert_eq!(again.pwd_hash.as_deref(), Some("argon2-set-here"));
}

/// The personal OWNER membership is accepted on creation. Without `accepted_at`,
/// `saas_auth::token::pick_org` skips it and login mints a token with no `org` claim,
/// so a fresh account cannot reach a single org-scoped route.
#[tokio::test]
async fn a_new_accounts_own_org_is_already_accepted() {
	let db = TmpDb::new("own-org-accepted");
	let store = setup(&db).await;

	let (account, org) = store.create_account(&new_account("owner@e.st"), &[]).await.unwrap();
	let orgs = store.orgs_for_account(account.id).await.unwrap();

	assert_eq!(orgs.len(), 1);
	assert_eq!(orgs[0].uid, org.uid);
	assert!(orgs[0].accepted_at.is_some(), "the owner does not invite themselves");
}

/// Enrolment used to stamp `confirmed_at` and write `recovery_hashes` in two statements
/// with N argon2 passes between them. A failure in the gap armed 2FA with no recovery codes
/// and returned none, and `enrol` refuses an already-confirmed credential — an unrecoverable
/// lockout. One statement is what makes it all-or-nothing.
#[tokio::test]
async fn confirming_a_factor_arms_it_and_stores_its_recovery_codes_together() {
	let db = TmpDb::new("confirm-atomic");
	let store = setup(&db).await;

	let (account, _) = store.create_account(&new_account("atomic@e.st"), &[]).await.unwrap();
	store
		.put_totp(&NewTotpCredential {
			account_id: account.id,
			secret_nonce: vec![0; 12],
			secret_enc: vec![0; 32],
			digits: 6,
			period: 30,
			recovery_hashes: "[]".to_owned(),
		})
		.await
		.unwrap();

	let unconfirmed = store.totp_by_account(account.id).await.unwrap().unwrap();
	assert!(unconfirmed.confirmed_at.is_none());
	assert_eq!(unconfirmed.recovery_hashes, "[]");

	store
		.confirm_totp(account.id, Timestamp::now(), r#"["h1","h2"]"#)
		.await
		.unwrap();

	let confirmed = store.totp_by_account(account.id).await.unwrap().unwrap();
	assert!(confirmed.confirmed_at.is_some(), "the factor is armed");
	assert_eq!(confirmed.recovery_hashes, r#"["h1","h2"]"#, "…and never without its codes");
}

/// `anonymize_account` reached out to the **reader** pool mid-transaction to ask whether
/// `billing_parties` exists — a read-modify-write across two connections, which is the pattern
/// this project bans. `has_table` now runs on the transaction itself. This is what proves the
/// erasure still reaches that table, and that every statement in the transaction lands
/// together: an erasure that half-applies is the worst outcome the method has.
///
/// A real file database, not `sqlite::memory:` — each in-memory *connection* is its own
/// database, so the two pools would not even see the same schema.
#[tokio::test]
async fn erasure_reaches_every_table_in_one_transaction() {
	let db = TmpDb::new("erasure-atomic");
	let store = setup(&db).await;
	let (account, org) = store.create_account(&new_account("erase@e.st"), &[]).await.unwrap();

	// A natural person's billing party in the account's own personal org — the one
	// cross-crate entry on the allowlist, and the branch `has_table` guards.
	sqlx::query(
		"INSERT INTO billing_parties (uid, org_id, kind, name, country, postcode, city,
		 street, email, created_at, updated_at)
		 VALUES ('prt_x', ?, 'P', 'Erase Me', 'HU', '1111', 'Budapest', 'Fo u. 1',
		 'erase@e.st', 0, 0)",
	)
	.bind(org.id)
	.execute(store.write_pool())
	.await
	.unwrap();

	store.anonymize_account(account.id, Timestamp::now(), &ERASURE).await.unwrap();

	let erased = store.account_by_id(account.id).await.unwrap().unwrap();
	assert_eq!(erased.status, saas_auth::store::AccountStatus::Anonymized);
	assert!(!erased.email.contains("erase@e.st"), "{}", erased.email);

	let org_name: String = sqlx::query_scalar(
		"SELECT name FROM orgs WHERE owner_account_id = ? AND kind = 'PERSONAL'",
	)
	.bind(account.id)
	.fetch_one(store.read_pool())
	.await
	.unwrap();
	assert_eq!(org_name, "[erased]");

	let party: (String, Option<String>) =
		sqlx::query_as("SELECT name, email FROM billing_parties WHERE uid = 'prt_x'")
			.fetch_one(store.read_pool())
			.await
			.unwrap();
	assert_eq!(party, ("[erased]".to_owned(), None), "the guarded branch must have run");
}

/// The allowlist value is **bound**, not interpolated: as a quoted SQL literal it would store
/// the four characters `'{}'`, text no `json_extract` can read.
#[tokio::test]
async fn erasure_blanks_object_body_to_valid_json() {
	let db = TmpDb::new("erasure-object-body");
	let store = setup(&db).await;
	let (account, org) = store.create_account(&new_account("blob@e.st"), &[]).await.unwrap();

	store
		.object_put(org.id, "invoice.ext", "inv_1", &json!({ "note": "Kiss Anna" }), &[])
		.await
		.unwrap();

	store.anonymize_account(account.id, Timestamp::now(), &ERASURE).await.unwrap();

	let blanked = store.object_get(org.id, "invoice.ext", "inv_1").await.unwrap().unwrap();
	assert_eq!(blanked.body, json!({}));
}

/// `record_login_failure` is a bare counter — the lockout ladder that built
/// `locked_until = CASE failed_logins + 1 …` at runtime is gone. Two things still matter to a
/// store adapter: the count goes up, and the `UPDATE` is unconditional, so the unknown-address
/// branch costs the same writer round trip and the route stays silent about which exist.
#[tokio::test]
async fn a_failed_login_counts_and_an_unknown_account_costs_the_same_write() {
	let db = TmpDb::new("failure-count");
	let store = setup(&db).await;
	let (account, _) = store.create_account(&new_account("ladder@e.st"), &[]).await.unwrap();

	// Twice, not three times: if the increment works at 2 it works at 3.
	for expected in 1..=2 {
		store.record_login_failure(account.id).await.unwrap();
		let reloaded = store.account_by_id(account.id).await.unwrap().unwrap();
		assert_eq!(reloaded.failed_logins, expected);
	}

	// `login::NO_ACCOUNT` — matches nothing, must still succeed rather than error out.
	store.record_login_failure(0).await.unwrap();
}

/// `revoke_api_key` resolved a `key_<ULID>` taken from a request body with no org
/// predicate, so any org could revoke any other org's key. The trait signature could
/// not even express the scope, so no caller was in a position to fix it.
#[tokio::test]
async fn one_org_cannot_revoke_another_orgs_api_key() {
	let db = TmpDb::new("api-key-org");
	let store = setup(&db).await;

	let (a, org_a) = store.create_account(&new_account("a@e.st"), &[]).await.unwrap();
	let (b, org_b) = store.create_account(&new_account("b@e.st"), &[]).await.unwrap();

	let key = async |org_id: i64, account_id: i64, prefix: &str| {
		store
			.create_api_key(
				&NewApiKey {
					org_id,
					account_id,
					name: format!("{prefix} key"),
					prefix: prefix.to_owned(),
					key_hash: format!("argon2-{prefix}"),
					scopes: "[]".to_owned(),
					expires_at: None,
				},
				i64::MAX,
			)
			.await
			.unwrap()
			.unwrap()
	};
	let key_a = key(org_a.id, a.id, "aaaaaaaa").await;
	let key_b = key(org_b.id, b.id, "bbbbbbbb").await;

	assert!(
		!store.revoke_api_key(org_a.id, &key_b.uid, Timestamp(1_000)).await.unwrap(),
		"another org's key is a miss, not a revocation"
	);
	assert!(
		store.api_key_by_prefix("bbbbbbbb").await.unwrap().unwrap().revoked_at.is_none(),
		"B's key has to stay live"
	);

	// An org's own key still revokes, and only once.
	assert!(store.revoke_api_key(org_a.id, &key_a.uid, Timestamp(1_000)).await.unwrap());
	assert!(!store.revoke_api_key(org_a.id, &key_a.uid, Timestamp(2_000)).await.unwrap());
	assert_eq!(
		store.api_key_by_prefix("aaaaaaaa").await.unwrap().unwrap().revoked_at,
		Some(Timestamp(1_000))
	);
}

/// The liveness read is one statement with three joins, and every column it returns is a way a
/// key dies without ever being revoked. A test that only revokes cannot see any of it: the join
/// *is* the feature, so the same row is driven through a removed membership, a suspended account
/// and a suspended org.
#[tokio::test]
async fn api_key_liveness_carries_membership_account_and_org() {
	let db = TmpDb::new("machine-key-liveness");
	let store = setup(&db).await;

	let root = store.root_org_id().await.unwrap();
	let (owner, _) = store.create_account(&new_account("keyowner@e.st"), &[]).await.unwrap();
	let (member, _) = store.create_account(&new_account("keymember@e.st"), &[]).await.unwrap();
	// A `MEMBER` on a shared org, not the account's own: `remove_membership` refuses an `OWNER`.
	let shared = store.create_org(OrgKind::Shared, root, "Kft.", owner.id, None).await.unwrap();
	store.put_membership(shared.id, member.id, Role::Member).await.unwrap();
	store.accept_membership(shared.id, member.id, Timestamp(1)).await.unwrap();
	// `create_account` leaves the account `PENDING` until activation; move it to `ACTIVE` so the
	// baseline is a genuinely live key and each later assertion is the transition that killed it.
	store.set_account_status(member.id, AccountStatus::Active).await.unwrap();
	// The value `create_api_key` returns is the same joined shape every read produces, so the
	// mint path's insert-then-read-back is asserted here rather than only through a later read.
	let minted = store
		.create_api_key(
			&NewApiKey {
				org_id: shared.id,
				account_id: member.id,
				name: "CI".to_owned(),
				prefix: "abcd1234".to_owned(),
				key_hash: "sha256-hex".to_owned(),
				scopes: "[\"invoice:read\"]".to_owned(),
				expires_at: None,
			},
			i64::MAX,
		)
		.await
		.unwrap()
		.unwrap();
	assert!(minted.member, "an accepted membership is what makes the minted key live");
	assert_eq!((minted.account_status.as_str(), minted.org_status.as_str()), ("ACTIVE", "ACTIVE"));

	let key = store.api_key_by_prefix("abcd1234").await.unwrap().unwrap();
	assert!(key.member, "an accepted membership is what makes the key live");
	assert_eq!((key.account_status.as_str(), key.org_status.as_str()), ("ACTIVE", "ACTIVE"));
	assert_eq!(key.scopes, "[\"invoice:read\"]", "the column reaches the caller verbatim");
	assert!(key.revoked_at.is_none() && key.expires_at.is_none());

	// The key row is untouched, so only the join can kill it.
	assert!(store.remove_membership(shared.id, member.id).await.unwrap());
	assert!(!store.api_key_by_prefix("abcd1234").await.unwrap().unwrap().member);

	// `remove_membership` deletes the row, so an `accept` alone updates nothing: the membership
	// has to be put back before it can be accepted again.
	store.put_membership(shared.id, member.id, Role::Member).await.unwrap();
	store.accept_membership(shared.id, member.id, Timestamp(2)).await.unwrap();
	assert!(store.api_key_by_prefix("abcd1234").await.unwrap().unwrap().member);

	// A suspended account and a suspended org must arrive as columns, never as a missing row:
	// `auth_mw` answers `E-AUTH-KEY-REVOKED` for both, and "unknown key" for neither.
	store.set_account_status(member.id, AccountStatus::Suspended).await.unwrap();
	let key = store.api_key_by_prefix("abcd1234").await.unwrap().unwrap();
	assert_eq!((key.account_status.as_str(), key.org_status.as_str()), ("SUSPENDED", "ACTIVE"));

	store.set_account_status(member.id, AccountStatus::Active).await.unwrap();
	store
		.update_org(shared.id, None, Patch::Undefined, Some(OrgStatus::Suspended))
		.await
		.unwrap();
	let key = store.api_key_by_prefix("abcd1234").await.unwrap().unwrap();
	assert_eq!((key.account_status.as_str(), key.org_status.as_str()), ("ACTIVE", "SUSPENDED"));
	assert!(key.member);
}

/// `set_account_status` was a bare `UPDATE accounts SET status = ? WHERE id = ?` with no
/// `CHECK`, no trigger and no precondition, so an operator un-suspending a batch — or a
/// consumer calling the trait directly — could put an anonymized account back to `ACTIVE`,
/// which `auth_mw::account_for_token` then accepts on an account whose email is
/// `anonymized+…@invalid` and whose `pwd_hash` is NULL. GDPR erasure has to be irreversible.
///
/// One layer only: the business-rule triggers are gone, so the guard is `set_account_status` alone;
/// raw SQL against `accounts` is the consumer's problem.
#[tokio::test]
async fn an_erased_account_cannot_be_brought_back() {
	use saas_auth::store::AccountStatus;

	let db = TmpDb::new("no-unerase");
	let store = setup(&db).await;
	let (account, _) = store.create_account(&new_account("erased@e.st"), &[]).await.unwrap();
	store.anonymize_account(account.id, Timestamp(1_000), &ERASURE).await.unwrap();

	for status in [AccountStatus::Active, AccountStatus::Pending, AccountStatus::Suspended] {
		let err = store.set_account_status(account.id, status).await.unwrap_err();
		assert_eq!(err.parts().1, "E-AUTH-ANONYMIZED", "{status:?} was accepted");
	}
	// The guard is narrow: an ordinary account still moves freely, and setting ANONYMIZED on
	// an already-anonymized one is a no-op rather than an error — `anonymize_account` is
	// idempotent and must stay so.
	let (live, _) = store.create_account(&new_account("live@e.st"), &[]).await.unwrap();
	store.set_account_status(live.id, AccountStatus::Active).await.unwrap();
	store.set_account_status(live.id, AccountStatus::Suspended).await.unwrap();
	store.set_account_status(live.id, AccountStatus::Active).await.unwrap();
	store.set_account_status(account.id, AccountStatus::Anonymized).await.unwrap();
}

/// Suspending revokes: `auth_mw` re-reads the epoch on privileged paths, but an ordinary read
/// carries on until `exp` otherwise. The service handle did the two writes separately, so a
/// `SQLITE_BUSY` on the second left the account `SUSPENDED` with every issued token still
/// resolving for up to 15 minutes — and no audit row. One transaction, or neither write.
#[tokio::test]
async fn suspending_an_account_bumps_its_token_epoch_atomically() {
	use saas_auth::store::AccountStatus;

	let db = TmpDb::new("suspend-epoch");
	let store = setup(&db).await;
	let (account, _org) = store.create_account(&new_account("suspend@e.st"), &[]).await.unwrap();
	let before = store.account_by_id(account.id).await.unwrap().unwrap().token_epoch;

	store.set_account_status(account.id, AccountStatus::Suspended).await.unwrap();
	let after = store.account_by_id(account.id).await.unwrap().unwrap();
	assert_eq!(after.status, AccountStatus::Suspended);
	assert_eq!(after.token_epoch, before + 1, "a suspension that did not revoke is the bug");

	// Every other status leaves the epoch alone — un-suspending must not sign anyone out.
	store.set_account_status(account.id, AccountStatus::Active).await.unwrap();
	let reactivated = store.account_by_id(account.id).await.unwrap().unwrap();
	assert_eq!(reactivated.status, AccountStatus::Active);
	assert_eq!(reactivated.token_epoch, before + 1);
}

/// `latest_consent` ordered by `at` while `list_consents` picks `MAX(id)` per scope, so two
/// grants whose clock order disagrees with their insertion order made `GET /api/consents`
/// report one row and `DELETE /api/consents/{kind}` withdraw the other. `id` is the key that
/// does not depend on the wall clock.
#[tokio::test]
async fn latest_consent_and_list_consents_agree_across_a_clock_step_back() {
	use saas_auth::store::{LegalKind, NewConsent};

	let db = TmpDb::new("consent-clock-step");
	let store = setup(&db).await;
	let (account, _org) = store.create_account(&new_account("consent@e.st"), &[]).await.unwrap();

	let grant = |version: &'static str| NewConsent {
		account_id: account.id,
		org_id: None,
		kind: LegalKind::Tos,
		legal_doc_id: None,
		doc_version: version.to_owned(),
		doc_sha256: format!("{:064x}", 0),
		granted: true,
		ip: None,
		user_agent: None,
	};
	store.record_consent(&grant("1"), Timestamp(2_000)).await.unwrap();
	// The clock stepped back between the two grants: the newer row carries the earlier `at`.
	let newest = store.record_consent(&grant("2"), Timestamp(1_000)).await.unwrap();

	let latest = store.latest_consent(account.id, LegalKind::Tos, None).await.unwrap().unwrap();
	let listed = store.list_consents(account.id).await.unwrap();
	assert_eq!(latest.id, newest);
	assert_eq!(listed.len(), 1);
	assert_eq!(listed[0].id, latest.id, "the list and the withdrawal must name one row");
}

async fn grant(store: &SqliteStore, org_id: i64, account_id: i64, role: &str) {
	sqlx::query(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES (?, ?, ?, 0, 0)",
	)
	.bind(org_id)
	.bind(account_id)
	.bind(role)
	.execute(store.write_pool())
	.await
	.unwrap();
}

/// Effective role is the **maximum** role held on an org or on any of its ancestors, so a
/// role granted high in the tree reaches every org below it and a lower direct grant cannot
/// take it away. Row filtering is untouched: only role resolution walks.
#[tokio::test]
async fn a_role_on_an_ancestor_resolves_on_every_descendant() {
	let db = TmpDb::new("ancestor-walk");
	let store = setup(&db).await;
	let root = store.root_org_id().await.unwrap();

	let (boss, _) = store.create_account(&new_account("boss@e.st"), &[]).await.unwrap();
	let (staff, _) = store.create_account(&new_account("staff@e.st"), &[]).await.unwrap();
	let (outsider, _) = store.create_account(&new_account("nobody@e.st"), &[]).await.unwrap();
	let parent = store
		.create_org(OrgKind::Shared, root, "Anya Kft.", boss.id, None)
		.await
		.unwrap();
	let child = store
		.create_org(OrgKind::Shared, parent.id, "Lanya Kft.", boss.id, None)
		.await
		.unwrap();

	grant(&store, parent.id, staff.id, "ADMIN").await;
	grant(&store, child.id, staff.id, "MEMBER").await;

	assert_eq!(store.org_role(staff.id, child.id).await.unwrap(), Some(Role::Admin));
	assert_eq!(
		store.org_membership_role(staff.id, child.uid.as_str()).await.unwrap(),
		Some((child.id, Role::Admin)),
		"the walk answers by uid too, in one round trip"
	);
	assert_eq!(store.org_role(outsider.id, child.id).await.unwrap(), None);
	assert_eq!(
		store.org_role(staff.id, parent.id).await.unwrap(),
		Some(Role::Admin),
		"the walk does not descend: a child grant never reaches its parent"
	);
}

/// `parent_id` is a plain nullable self-reference, so nothing in the schema stops an operator
/// from closing a loop. The `LIMIT 16` in the recursive CTE is the standing guard: the walk
/// returns rather than spinning.
#[tokio::test]
async fn the_ancestor_walk_terminates_on_a_cycle() {
	let db = TmpDb::new("ancestor-cycle");
	let store = setup(&db).await;
	let root = store.root_org_id().await.unwrap();

	let (boss, _) = store.create_account(&new_account("boss@e.st"), &[]).await.unwrap();
	let a = store.create_org(OrgKind::Shared, root, "A Kft.", boss.id, None).await.unwrap();
	let b = store.create_org(OrgKind::Shared, a.id, "B Kft.", boss.id, None).await.unwrap();
	sqlx::query("UPDATE orgs SET parent_id = ? WHERE id = ?")
		.bind(b.id)
		.bind(a.id)
		.execute(store.write_pool())
		.await
		.unwrap();

	assert_eq!(store.org_role(boss.id, b.id).await.unwrap(), Some(Role::Owner));
}

/// Every authenticated request runs these two statements, where it used to run two point
/// lookups. `memberships` is `PRIMARY KEY (org_id, account_id) WITHOUT ROWID` with
/// `idx_membership_account`, `orgs.uid` is UNIQUE and `idx_org_root` covers the kind probe —
/// drop one and the hottest path in the framework quietly becomes a scan, with nothing else
/// failing.
///
/// The two statements are copies of `core.rs`'s `account_for_token` and `org_membership_role`
/// (`ancestors`/`BEST_ROLE` are `pub(crate)`), so they move together.
#[tokio::test]
async fn the_per_request_auth_queries_do_not_scan() {
	let db = TmpDb::new("auth-plans");
	let store = setup(&db).await;

	let account_for_token = "SELECT a.id, a.token_epoch, \
	        EXISTS (SELECT 1 FROM memberships m \
	                 WHERE m.account_id = a.id \
	                   AND m.org_id = (SELECT id FROM orgs WHERE kind = 'ROOT') \
	                   AND m.role IN ('ADMIN', 'OWNER') \
	                   AND m.accepted_at IS NOT NULL), \
	        a.status \
	   FROM accounts a WHERE a.uid = ?";
	let org_membership_role = "WITH RECURSIVE anc(id, parent_id, depth) AS ( \
	         SELECT id, parent_id, 0 FROM orgs WHERE uid = ? AND status = 'ACTIVE' \
	   UNION ALL \
	         SELECT o.id, o.parent_id, anc.depth + 1 FROM orgs o JOIN anc ON o.id = anc.parent_id \
	          WHERE o.status = 'ACTIVE' \
	   LIMIT 16 \
	 ) SELECT o.id, (SELECT MAX(CASE m.role WHEN 'OWNER' THEN 3 WHEN 'ADMIN' THEN 2 ELSE 1 END) \
	   FROM memberships m JOIN anc ON m.org_id = anc.id \
	  WHERE m.account_id = ? AND m.accepted_at IS NOT NULL) \
	   FROM orgs o WHERE o.uid = ? AND o.status = 'ACTIVE'";

	let cases = [
		("account_for_token", account_for_token, 1),
		("org_membership_role", org_membership_role, 3),
	];
	for (what, sql, binds) in cases {
		let mut q = sqlx::query_as::<_, (i64, i64, i64, String)>(sqlx::AssertSqlSafe(format!(
			"EXPLAIN QUERY PLAN {sql}"
		)));
		for _ in 0..binds {
			q = q.bind(1_i64);
		}
		for (_, _, _, detail) in q.fetch_all(store.read_pool()).await.unwrap() {
			assert!(
				!detail.contains("SCAN memberships") && !detail.contains("SCAN orgs"),
				"{what} scans: {detail}"
			);
		}
	}
}

/// The root org's `owner_account_id` is `NULL`, so the "other members" count compared
/// against `NULL` never counted them: the platform root was deletable.
#[tokio::test]
async fn the_root_org_is_never_deletable() {
	let db = TmpDb::new("root-undeletable");
	let store = setup(&db).await;
	let root = store.root_org_id().await.unwrap();
	for email in ["a@e.st", "b@e.st"] {
		let (account, _) = store.create_account(&new_account(email), &[]).await.unwrap();
		store.put_membership(root, account.id, Role::Member).await.unwrap();
		store.accept_membership(root, account.id, Timestamp::now()).await.unwrap();
	}
	assert!(!store.delete_org(root).await.unwrap(), "the root is not deletable");
	assert!(store.org_by_id(root).await.unwrap().is_some());
}

/// `delete_org` pre-checked `invoices` and `consents` only, so a `sellers`, `services` or
/// `payments` row, or a child org, made the delete raise an FK error the service read as a 500.
#[tokio::test]
async fn a_non_cascading_reference_keeps_an_org_undeletable() {
	let db = TmpDb::new("retention-refs");
	let store = setup(&db).await;
	let root = store.root_org_id().await.unwrap();
	let (owner, _) = store.create_account(&new_account("refs@e.st"), &[]).await.unwrap();

	let parent = store
		.create_org(OrgKind::Shared, root, "Parent Kft.", owner.id, None)
		.await
		.unwrap();
	store
		.create_org(OrgKind::Shared, parent.id, "Child Kft.", owner.id, None)
		.await
		.unwrap();
	assert!(!store.delete_org(parent.id).await.unwrap(), "a child org keeps the parent");

	let with_service = store
		.create_org(OrgKind::Shared, root, "Svc Kft.", owner.id, None)
		.await
		.unwrap();
	sqlx::query(
		"INSERT INTO services (uid, org_id, code, name, unit_price, vat_code, created_at, updated_at)
		 VALUES ('svc_x', ?, 'C', 'Consulting', 100, 'STD27', 0, 0)",
	)
	.bind(with_service.id)
	.execute(store.write_pool())
	.await
	.unwrap();
	assert!(!store.delete_org(with_service.id).await.unwrap());

	let with_seller = store
		.create_org(OrgKind::Shared, root, "Seller Kft.", owner.id, None)
		.await
		.unwrap();
	sqlx::query(
		"INSERT INTO sellers (id, uid, org_id, nav_base_url, created_at)
		 VALUES (50, 'sel_x', ?, '', 0)",
	)
	.bind(with_seller.id)
	.execute(store.write_pool())
	.await
	.unwrap();
	assert!(!store.delete_org(with_seller.id).await.unwrap());

	// The one that used to be a constraint error rather than a refusal.
	let with_payment = store
		.create_org(OrgKind::Shared, root, "Pay Kft.", owner.id, None)
		.await
		.unwrap();
	sqlx::query(
		"INSERT INTO payments (uid, org_id, kind, amount, currency, created_at, updated_at)
		 VALUES ('pay_x', ?, 'MANUAL', 0, 'HUF', 0, 0)",
	)
	.bind(with_payment.id)
	.execute(store.write_pool())
	.await
	.unwrap();
	assert!(!store.delete_org(with_payment.id).await.unwrap());

	// `objects` is guarded *because* it cascades: without the check the bodies would go with the
	// org, silently and with nothing recording what went.
	let with_object = store
		.create_org(OrgKind::Shared, root, "Obj Kft.", owner.id, None)
		.await
		.unwrap();
	store
		.object_put(with_object.id, "booking", "bk_1", &json!({"a": 1}), &[])
		.await
		.unwrap();
	assert!(!store.delete_org(with_object.id).await.unwrap());
	assert!(store.org_by_id(with_object.id).await.unwrap().is_some());
	assert!(store.object_get(with_object.id, "booking", "bk_1").await.unwrap().is_some());

	let with_sub = store
		.create_org(OrgKind::Shared, root, "Sub Kft.", owner.id, None)
		.await
		.unwrap();
	sqlx::query(
		"INSERT INTO services (id, uid, org_id, code, name, unit_price, vat_code, created_at, updated_at)
		 VALUES (70, 'svc_sub', ?, 'P', 'Plan', 100, 'STD27', 0, 0)",
	)
	.bind(with_service.id)
	.execute(store.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO offers (id, uid, seller_org_id, code, name, kind, service_id, created_at, updated_at)
		 VALUES (71, 'ofr_x', ?, 'P', 'Plan', 'ONE_TIME', 70, 0, 0)",
	)
	.bind(with_service.id)
	.execute(store.write_pool())
	.await
	.unwrap();
	sqlx::query(
		"INSERT INTO subscriptions (uid, org_id, offer_id, status, currency, price, period_start,
		   period_end, pay_method, created_at, updated_at)
		 VALUES ('sub_x', ?, 71, 'ACTIVE', 'HUF', 0, 0, 1, 'CARD', 0, 0)",
	)
	.bind(with_sub.id)
	.execute(store.write_pool())
	.await
	.unwrap();
	assert!(!store.delete_org(with_sub.id).await.unwrap(), "a subscription keeps the org");

	// And the guard is per table, not a blanket refusal: an org holding none of them goes.
	let empty = store
		.create_org(OrgKind::Shared, root, "Empty Kft.", owner.id, None)
		.await
		.unwrap();
	assert!(store.delete_org(empty.id).await.unwrap());
}

/// `refs.org_id` does not cascade and every invite mints an `org_invite` ref, so deleting an org
/// that ever invited anyone raised an FK error the service read as a 500.
#[tokio::test]
async fn an_org_that_sent_an_invite_is_deletable() {
	use saas_core::refs::{NewRef, RefStore};
	let db = TmpDb::new("invite-deletable");
	let store = setup(&db).await;
	let root = store.root_org_id().await.unwrap();
	let (owner, _) = store.create_account(&new_account("inv@e.st"), &[]).await.unwrap();
	let org = store
		.create_org(OrgKind::Shared, root, "Invite Kft.", owner.id, None)
		.await
		.unwrap();
	store
		.ref_insert(&NewRef {
			uid: saas_core::ids::RefId::generate(),
			code: "inv-code".to_owned(),
			ref_type: "org_invite".to_owned(),
			org_id: org.id,
			created_by: Some(owner.id),
			target: None,
			email: Some("guest@e.st".to_owned()),
			params: json!({}),
			uses_left: Some(1),
			expires_at: None,
		})
		.await
		.unwrap();
	assert!(store.delete_org(org.id).await.unwrap());
	assert!(store.refs_of_org(org.id, None).await.unwrap().is_empty());
}

/// All three ancestor walks anchor on `status = 'ACTIVE'`, so a suspended root would strip
/// every inherited role at once — including the operator authority that is the only way back.
#[tokio::test]
async fn the_root_org_cannot_be_suspended() {
	let db = TmpDb::new("root-unsuspendable");
	let store = setup(&db).await;
	let root = store.root_org_id().await.unwrap();
	let (owner, _) = store.create_account(&new_account("suspend@e.st"), &[]).await.unwrap();
	let shared = store.create_org(OrgKind::Shared, root, "Kft.", owner.id, None).await.unwrap();

	let err = store
		.update_org(root, None, Patch::Undefined, Some(OrgStatus::Suspended))
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-CONFLICT", "{err:?}");
	assert_eq!(store.org_by_id(root).await.unwrap().unwrap().status, OrgStatus::Active);

	store
		.update_org(shared.id, None, Patch::Undefined, Some(OrgStatus::Suspended))
		.await
		.unwrap();
	assert_eq!(store.org_by_id(shared.id).await.unwrap().unwrap().status, OrgStatus::Suspended);
}

/// `idx_org_root` makes `kind = 'ROOT'` single-row; `parent_id IS NULL` is not constrained by
/// anything, so a read keyed on it can land on an unrelated parentless org.
#[tokio::test]
async fn the_root_is_found_by_kind_not_by_being_parentless() {
	let db = TmpDb::new("root-by-kind");
	let store = setup(&db).await;
	sqlx::query(
		"INSERT INTO orgs (uid, parent_id, kind, name, created_at)
			VALUES ('org_shadow', NULL, 'SHARED', 'Shadow Kft.', 0)",
	)
	.execute(store.write_pool())
	.await
	.unwrap();
	let shadow: i64 = sqlx::query_scalar("SELECT id FROM orgs WHERE uid = 'org_shadow'")
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	let seeded: i64 = sqlx::query_scalar("SELECT id FROM orgs WHERE kind = 'ROOT'")
		.fetch_one(store.read_pool())
		.await
		.unwrap();
	let root = store.root_org_id().await.unwrap();
	assert_eq!(root, seeded);
	assert_ne!(root, shadow);
	assert_eq!(store.root_org_id().await.unwrap(), root);
}

/// A key minted on an org its holder reaches only through an ancestor. `Auth::create_api_key`
/// authorizes with `org_role`, an ancestor walk, so the mint succeeds — and the per-request
/// `member` flag was a direct-membership `EXISTS`, so the key was dead on arrival.
#[tokio::test]
async fn a_key_on_a_child_org_is_live_through_an_ancestor_membership() {
	let db = TmpDb::new("machine-key-ancestor");
	let store = setup(&db).await;

	let root = store.root_org_id().await.unwrap();
	let (owner, _) = store.create_account(&new_account("anc-owner@e.st"), &[]).await.unwrap();
	let (member, _) = store.create_account(&new_account("anc-member@e.st"), &[]).await.unwrap();
	store.set_account_status(member.id, AccountStatus::Active).await.unwrap();

	// The membership lives on the parent; the key lives on the child.
	let parent = store
		.create_org(OrgKind::Shared, root, "Parent Kft.", owner.id, None)
		.await
		.unwrap();
	store.put_membership(parent.id, member.id, Role::Member).await.unwrap();
	store.accept_membership(parent.id, member.id, Timestamp(1)).await.unwrap();
	let child = store
		.create_org(OrgKind::Shared, parent.id, "Unit", owner.id, None)
		.await
		.unwrap();

	let key = store
		.create_api_key(
			&NewApiKey {
				org_id: child.id,
				account_id: member.id,
				name: "CI".to_owned(),
				prefix: "eeeeeeee".to_owned(),
				key_hash: "sha256-hex".to_owned(),
				scopes: "[\"invoice:read\"]".to_owned(),
				expires_at: None,
			},
			i64::MAX,
		)
		.await
		.unwrap()
		.unwrap();
	assert!(key.member, "an accepted membership on an ancestor is what makes the key live");
	// The two agree: the mint authorized on this walk, and `member` now reads the same one.
	assert_eq!(store.org_role(member.id, child.id).await.unwrap(), Some(Role::Member));

	// The membership is the parent's, so suspending the parent has to kill the key even though
	// the key's own org is still ACTIVE — the same walk `org_role` does for a browser session.
	store
		.update_org(parent.id, None, Patch::Undefined, Some(OrgStatus::Suspended))
		.await
		.unwrap();
	let key = store.api_key_by_prefix("eeeeeeee").await.unwrap().unwrap();
	assert!(!key.member, "a suspended ancestor still carried the membership");
	assert_eq!(key.org_status.as_str(), "ACTIVE", "the child org itself is untouched");
	assert_eq!(store.org_role(member.id, child.id).await.unwrap(), None, "and the session agrees");
	store
		.update_org(parent.id, None, Patch::Undefined, Some(OrgStatus::Active))
		.await
		.unwrap();

	// And the key dies when the ancestor membership goes.
	assert!(store.remove_membership(parent.id, member.id).await.unwrap());
	assert!(!store.api_key_by_prefix("eeeeeeee").await.unwrap().unwrap().member);
}

/// The passkey store round trip: enrol, look up by credential id (the usernameless-login path),
/// list per account, rename, touch, delete — and the duplicate-`credential_id` refusal, which is
/// what stops one authenticator's key from resolving to two accounts.
#[tokio::test]
async fn a_webauthn_credential_round_trips_and_a_duplicate_id_conflicts() {
	let db = TmpDb::new("webauthn-round-trip");
	let store = setup(&db).await;
	let (alice, _) = store.create_account(&new_account("alice@e.st"), &[]).await.unwrap();
	let (bob, _) = store.create_account(&new_account("bob@e.st"), &[]).await.unwrap();

	let new = |account_id: i64, credential_id: &str, credential: &str| NewWebauthnCredential {
		account_id,
		credential_id: credential_id.to_owned(),
		credential: credential.to_owned(),
		name: "This device".to_owned(),
		created_at: Timestamp(10),
	};
	let put = store
		.put_webauthn_credential(&new(alice.id, "cred-alice", "{\"counter\":0}"), i64::MAX)
		.await
		.unwrap()
		.unwrap();
	assert_eq!(put.account_id, alice.id);
	assert_eq!(put.name, "This device");
	assert_eq!(put.last_used_at, None);

	let by_id = store.webauthn_by_credential_id("cred-alice").await.unwrap().unwrap();
	assert_eq!(by_id.credential_id, "cred-alice");
	assert_eq!(store.webauthn_for_account(alice.id).await.unwrap().len(), 1);
	assert!(store.webauthn_for_account(bob.id).await.unwrap().is_empty());

	// Only the owner may rename.
	assert!(!store.rename_webauthn(bob.id, "cred-alice", "stolen").await.unwrap());
	assert!(store.rename_webauthn(alice.id, "cred-alice", "YubiKey").await.unwrap());
	assert_eq!(
		store.webauthn_by_credential_id("cred-alice").await.unwrap().unwrap().name,
		"YubiKey"
	);

	// One statement writes the counter and the timestamp, which describe the same assertion.
	store
		.touch_webauthn("cred-alice", "{\"counter\":1}", Timestamp(20))
		.await
		.unwrap();
	let touched = store.webauthn_by_credential_id("cred-alice").await.unwrap().unwrap();
	assert_eq!(touched.credential, "{\"counter\":1}");
	assert_eq!(touched.last_used_at, Some(Timestamp(20)));

	// A second account cannot register the same credential id.
	let dup = store
		.put_webauthn_credential(&new(bob.id, "cred-alice", "{}"), i64::MAX)
		.await
		.unwrap_err();
	assert_eq!(dup.parts().1, "E-CORE-CONFLICT");

	// Only the owner may delete, and deleting twice is false the second time.
	assert!(!store.delete_webauthn(bob.id, "cred-alice").await.unwrap());
	assert!(store.delete_webauthn(alice.id, "cred-alice").await.unwrap());
	assert!(!store.delete_webauthn(alice.id, "cred-alice").await.unwrap());
	assert!(store.webauthn_by_credential_id("cred-alice").await.unwrap().is_none());
}

/// The count and the insert share one statement, like the API-key cap: checked at the challenge
/// alone, two concurrent `register/challenge` calls both pass and the account overshoots it.
#[tokio::test]
async fn put_webauthn_credential_refuses_past_the_cap() {
	let db = TmpDb::new("webauthn-cap");
	let store = setup(&db).await;
	let (alice, _) = store.create_account(&new_account("cap@e.st"), &[]).await.unwrap();
	let new = |credential_id: &str| NewWebauthnCredential {
		account_id: alice.id,
		credential_id: credential_id.to_owned(),
		credential: "{}".to_owned(),
		name: "This device".to_owned(),
		created_at: Timestamp(10),
	};
	assert!(store.put_webauthn_credential(&new("cred-one"), 1).await.unwrap().is_some());
	assert!(store.put_webauthn_credential(&new("cred-two"), 1).await.unwrap().is_none());
}

/// "Live" is unrevoked *and* unexpired, as the setting's own text says: counting expired rows
/// refused a mint with "at most 20 live API keys per org" for keys that lapsed years ago.
#[tokio::test]
async fn an_expired_api_key_does_not_count_against_the_live_cap() {
	let db = TmpDb::new("api-key-live-cap");
	let store = setup(&db).await;
	let root = store.root_org_id().await.unwrap();
	let (owner, _) = store.create_account(&new_account("caplive@e.st"), &[]).await.unwrap();
	let org = store.create_org(OrgKind::Shared, root, "Kft.", owner.id, None).await.unwrap();

	let key = |prefix: &str| NewApiKey {
		org_id: org.id,
		account_id: owner.id,
		name: "CI".to_owned(),
		prefix: prefix.to_owned(),
		key_hash: "sha256-hex".to_owned(),
		scopes: "[]".to_owned(),
		expires_at: None,
	};
	assert!(store.create_api_key(&key("aaaaaaaa"), 1).await.unwrap().is_some());
	assert!(
		store.create_api_key(&key("bbbbbbbb"), 1).await.unwrap().is_none(),
		"a second live key must hit the cap"
	);

	// Expired, not revoked: no longer live, so it frees the slot.
	sqlx::query("UPDATE api_keys SET expires_at = ? WHERE prefix = ?")
		.bind(Timestamp::now().0 - 1)
		.bind("aaaaaaaa")
		.execute(store.write_pool())
		.await
		.unwrap();
	assert!(
		store.create_api_key(&key("cccccccc"), 1).await.unwrap().is_some(),
		"an expired key was counted as live"
	);
}

// vim: ts=4
