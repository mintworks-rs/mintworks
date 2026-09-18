//! `AuthStore` integration tests for the store-level authorization and account-safety
//! guarantees: GDPR erasure scope and irreversibility, membership revocation, the TOTP
//! compare-and-swaps, activation's first password, and API-key tenant scoping.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use saas_auth::store::{
	AuthStore, ErasurePlan, NewAccount, NewApiKey, NewTotpCredential, Role, TenantKind,
};
use saas_core::{config::Config, prelude::*};
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
		tenant_name: email.to_owned(),
	}
}

async fn add_party(store: &SqliteStore, tenant_id: i64, uid: &str) {
	sqlx::query(
		"INSERT INTO billing_parties (uid, tenant_id, kind, name, country, city, created_at,
			updated_at)
		 VALUES (?, ?, 'P', 'Kiss Anna', 'HU', 'Budapest', 0, 0)",
	)
	.bind(uid)
	.bind(tenant_id)
	.execute(store.writer())
	.await
	.unwrap();
}

async fn party_name(store: &SqliteStore, uid: &str) -> String {
	sqlx::query_scalar("SELECT name FROM billing_parties WHERE uid = ?")
		.bind(uid)
		.fetch_one(store.reader())
		.await
		.unwrap()
}

/// The allowlist itself is `saas_auth::gdpr::ERASURE` and is `pub(crate)` to that crate — what
/// the adapter owes is honouring *whatever* plan it is handed, and the `kind = 'P'` scoping it
/// cannot read off the plan. So the suite brings its own, shaped like the real one.
const ERASURE: ErasurePlan = ErasurePlan {
	accounts: &[("name", None), ("pwd_hash", None)],
	tenants: &[("name", Some("[erased]"))],
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
/// pre-checks this on the reader pool for the message, so a `POST /api/tenants` landing between
/// that read and this transaction anonymized the owner of a live organisation: `remove_member`
/// refuses to remove an `OWNER` and `set_member_role` refuses to assign one, so no route
/// recovers it. Erasing an owner is also no licence to destroy the organisation's customer
/// records, which are other people's data.
#[tokio::test]
async fn anonymize_account_refuses_an_account_that_still_owns_an_organisation() {
	let db = TmpDb::new("erasure-scope");
	let store = setup(&db).await;

	let (account, personal) =
		store.create_account(&new_account("owner@e.st"), &[], None).await.unwrap();
	let org = store
		.create_tenant(TenantKind::Organisation, "Céges Kft.", account.id, None)
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

/// A key belongs to the person, not to a tenant. Scoped to the subject's *personal* tenant,
/// erasure left a mere member's organisation-scoped keys live while `gdpr`'s module doc
/// promised "every key revoked".
#[tokio::test]
async fn erasure_revokes_the_subjects_organisation_keys() {
	let db = TmpDb::new("erasure-api-keys");
	let store = setup(&db).await;

	let (owner, _) = store.create_account(&new_account("owner@e.st"), &[], None).await.unwrap();
	let (member, _) = store.create_account(&new_account("member@e.st"), &[], None).await.unwrap();
	let org = store
		.create_tenant(TenantKind::Organisation, "Céges Kft.", owner.id, None)
		.await
		.unwrap();
	store.put_membership(org.id, member.id, Role::Member).await.unwrap();
	store
		.create_api_key(&NewApiKey {
			tenant_id: org.id,
			account_id: member.id,
			name: "member key".to_owned(),
			prefix: "mmmmmmmm".to_owned(),
			key_hash: "argon2-member".to_owned(),
			scopes: "[]".to_owned(),
			expires_at: None,
		})
		.await
		.unwrap();

	store.anonymize_account(member.id, Timestamp(1_000), &ERASURE).await.unwrap();

	let key = store.api_key_by_prefix("mmmmmmmm").await.unwrap().unwrap();
	assert_eq!(key.revoked_at, Some(Timestamp(1_000)));
}

/// Removal ends *this* tenant and nothing else. The middleware's tenant lookup must stop
/// resolving, and `token_epoch` must stay put: it is account-wide, so bumping it would let
/// one tenant's admin sign the account out of every other tenant it belongs to.
#[tokio::test]
async fn removing_a_membership_revokes_the_member_at_once() {
	let db = TmpDb::new("membership-revoke");
	let store = setup(&db).await;

	let (owner, _) = store.create_account(&new_account("owner@e.st"), &[], None).await.unwrap();
	let (member, _) = store.create_account(&new_account("member@e.st"), &[], None).await.unwrap();
	let org = store
		.create_tenant(TenantKind::Organisation, "Céges Kft.", owner.id, None)
		.await
		.unwrap();
	store.put_membership(org.id, member.id, Role::Member).await.unwrap();

	// The exact lookup `saas_core::auth_mw` does to turn a `tnt` claim into `Ctx.tenant_id`.
	let resolves = async |account_id: i64| -> Option<i64> {
		sqlx::query_scalar::<_, i64>(
			"SELECT t.id FROM tenants t
			 JOIN memberships m ON m.tenant_id = t.id
			 WHERE t.uid = ? AND m.account_id = ?",
		)
		.bind(org.uid.as_str())
		.bind(account_id)
		.fetch_optional(store.reader())
		.await
		.unwrap()
	};

	assert_eq!(resolves(member.id).await, Some(org.id));
	let before = store.account_by_id(member.id).await.unwrap().unwrap().token_epoch;

	assert!(store.remove_membership(org.id, member.id).await.unwrap());

	assert_eq!(resolves(member.id).await, None, "a removed member must not resolve the tenant");
	let after = store.account_by_id(member.id).await.unwrap().unwrap().token_epoch;
	assert_eq!(after, before, "a membership change must not sign the account out everywhere");

	// A no-op removal changes nothing either.
	assert!(!store.remove_membership(org.id, member.id).await.unwrap());
	let again = store.account_by_id(member.id).await.unwrap().unwrap().token_epoch;
	assert_eq!(again, before);

	// And the same account's *other* tenants keep their membership: it is the removed tenant
	// that ends, not the member.
	let other = store
		.create_tenant(TenantKind::Organisation, "Masik Kft.", owner.id, None)
		.await
		.unwrap();
	store.put_membership(other.id, member.id, Role::Member).await.unwrap();
	store.accept_membership(other.id, member.id, Timestamp::now()).await.unwrap();
	assert!(!store.remove_membership(org.id, member.id).await.unwrap());
	assert_eq!(
		store.accepted_membership_role(other.id, member.id).await.unwrap(),
		Some(Role::Member),
		"removing one membership must leave the account's other tenants alone"
	);
}

/// `set_member_role`, `remove_member` and `attach_member` guard the owner by reading
/// `membership_role` off the **reader pool**, where a `transfer_tenant_ownership` that has not
/// committed yet is invisible, and then wrote unconditionally. The loser's write then left
/// `tenants.owner_account_id` pointing at an account with no membership row, and `owner_of`
/// answers `E-CORE-NOTFOUND` for everyone — no transfer, no deletion, no erasure, ever.
#[tokio::test]
async fn the_owner_membership_survives_a_concurrent_remove() {
	let db = TmpDb::new("owner-membership-race");
	let store = setup(&db).await;

	let (owner, _) = store.create_account(&new_account("owner@e.st"), &[], None).await.unwrap();
	let (member, _) = store.create_account(&new_account("member@e.st"), &[], None).await.unwrap();
	let org = store
		.create_tenant(TenantKind::Organisation, "Céges Kft.", owner.id, None)
		.await
		.unwrap();
	store.put_membership(org.id, member.id, Role::Member).await.unwrap();
	store.accept_membership(org.id, member.id, Timestamp::now()).await.unwrap();

	assert!(store.transfer_tenant_ownership(org.id, owner.id, member.id).await.unwrap());

	// Both are the write the losing request would have issued after its stale read.
	assert!(!store.remove_membership(org.id, member.id).await.unwrap());
	assert!(!store.put_membership(org.id, member.id, Role::Member).await.unwrap());
	assert_eq!(
		store.accepted_membership_role(org.id, member.id).await.unwrap(),
		Some(Role::Owner),
		"the tenant must stay administrable"
	);
}

/// The compare-and-swap behind `totp::spend_recovery`. Both racing requests read the same
/// array; only the one that writes first may win, or a code spent by one is resurrected by
/// the other.
#[tokio::test]
async fn a_recovery_code_set_can_only_be_swapped_once() {
	let db = TmpDb::new("recovery-cas");
	let store = setup(&db).await;

	let (account, _) = store.create_account(&new_account("2fa@e.st"), &[], None).await.unwrap();
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
			.fetch_one(store.reader())
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

	let (account, _) = store.create_account(&new_account("wipe@e.st"), &[], None).await.unwrap();
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
	.fetch_one(store.reader())
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

	let (account, _) = store.create_account(&new_account("replay@e.st"), &[], None).await.unwrap();
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
	let (account, _) = store.create_account(&invited, &[], None).await.unwrap();
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
/// `saas_auth::token::pick_tenant` skips it and login mints a token with no `tnt` claim,
/// so a fresh account cannot reach a single tenant-scoped route.
#[tokio::test]
async fn a_new_accounts_own_tenant_is_already_accepted() {
	let db = TmpDb::new("own-tenant-accepted");
	let store = setup(&db).await;

	let (account, tenant) =
		store.create_account(&new_account("owner@e.st"), &[], None).await.unwrap();
	let tenants = store.tenants_for_account(account.id).await.unwrap();

	assert_eq!(tenants.len(), 1);
	assert_eq!(tenants[0].uid, tenant.uid);
	assert!(tenants[0].accepted_at.is_some(), "the owner does not invite themselves");
}

/// Enrolment used to stamp `confirmed_at` and write `recovery_hashes` in two statements
/// with N argon2 passes between them. A failure in the gap armed 2FA with no recovery codes
/// and returned none, and `enrol` refuses an already-confirmed credential — an unrecoverable
/// lockout. One statement is what makes it all-or-nothing.
#[tokio::test]
async fn confirming_a_factor_arms_it_and_stores_its_recovery_codes_together() {
	let db = TmpDb::new("confirm-atomic");
	let store = setup(&db).await;

	let (account, _) = store.create_account(&new_account("atomic@e.st"), &[], None).await.unwrap();
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
	let (account, tenant) =
		store.create_account(&new_account("erase@e.st"), &[], None).await.unwrap();

	// A natural person's billing party in the account's own personal tenant — the one
	// cross-crate entry on the allowlist, and the branch `has_table` guards.
	sqlx::query(
		"INSERT INTO billing_parties (uid, tenant_id, kind, name, country, postcode, city,
		 street, email, created_at, updated_at)
		 VALUES ('prt_x', ?, 'P', 'Erase Me', 'HU', '1111', 'Budapest', 'Fo u. 1',
		 'erase@e.st', 0, 0)",
	)
	.bind(tenant.id)
	.execute(store.writer())
	.await
	.unwrap();

	store.anonymize_account(account.id, Timestamp::now(), &ERASURE).await.unwrap();

	let erased = store.account_by_id(account.id).await.unwrap().unwrap();
	assert_eq!(erased.status, saas_auth::store::AccountStatus::Anonymized);
	assert!(!erased.email.contains("erase@e.st"), "{}", erased.email);

	let tenant_name: String =
		sqlx::query_scalar("SELECT name FROM tenants WHERE owner_account_id = ? AND kind = 'P'")
			.bind(account.id)
			.fetch_one(store.reader())
			.await
			.unwrap();
	assert_eq!(tenant_name, "[erased]");

	let party: (String, Option<String>) =
		sqlx::query_as("SELECT name, email FROM billing_parties WHERE uid = 'prt_x'")
			.fetch_one(store.reader())
			.await
			.unwrap();
	assert_eq!(party, ("[erased]".to_owned(), None), "the guarded branch must have run");
}

/// `record_login_failure` is a bare counter — the lockout ladder that built
/// `locked_until = CASE failed_logins + 1 …` at runtime is gone. Two things still matter to a
/// store adapter: the count goes up, and the `UPDATE` is unconditional, so the unknown-address
/// branch costs the same writer round trip and the route stays silent about which exist.
#[tokio::test]
async fn a_failed_login_counts_and_an_unknown_account_costs_the_same_write() {
	let db = TmpDb::new("failure-count");
	let store = setup(&db).await;
	let (account, _) = store.create_account(&new_account("ladder@e.st"), &[], None).await.unwrap();

	// Twice, not three times: if the increment works at 2 it works at 3.
	for expected in 1..=2 {
		store.record_login_failure(account.id).await.unwrap();
		let reloaded = store.account_by_id(account.id).await.unwrap().unwrap();
		assert_eq!(reloaded.failed_logins, expected);
	}

	// `login::NO_ACCOUNT` — matches nothing, must still succeed rather than error out.
	store.record_login_failure(0).await.unwrap();
}

/// `revoke_api_key` resolved a `key_<ULID>` taken from a request body with no tenant
/// predicate, so any tenant could revoke any other tenant's key. The trait signature could
/// not even express the scope, so no caller was in a position to fix it.
#[tokio::test]
async fn one_tenant_cannot_revoke_another_tenants_api_key() {
	let db = TmpDb::new("api-key-tenant");
	let store = setup(&db).await;

	let (a, tenant_a) = store.create_account(&new_account("a@e.st"), &[], None).await.unwrap();
	let (b, tenant_b) = store.create_account(&new_account("b@e.st"), &[], None).await.unwrap();

	let key = async |tenant_id: i64, account_id: i64, prefix: &str| {
		store
			.create_api_key(&NewApiKey {
				tenant_id,
				account_id,
				name: format!("{prefix} key"),
				prefix: prefix.to_owned(),
				key_hash: format!("argon2-{prefix}"),
				scopes: "[]".to_owned(),
				expires_at: None,
			})
			.await
			.unwrap()
	};
	let key_a = key(tenant_a.id, a.id, "aaaaaaaa").await;
	let key_b = key(tenant_b.id, b.id, "bbbbbbbb").await;

	assert!(
		!store.revoke_api_key(tenant_a.id, &key_b.uid, Timestamp(1_000)).await.unwrap(),
		"another tenant's key is a miss, not a revocation"
	);
	assert!(
		store.api_key_by_prefix("bbbbbbbb").await.unwrap().unwrap().revoked_at.is_none(),
		"B's key has to stay live"
	);

	// A tenant's own key still revokes, and only once.
	assert!(store.revoke_api_key(tenant_a.id, &key_a.uid, Timestamp(1_000)).await.unwrap());
	assert!(!store.revoke_api_key(tenant_a.id, &key_a.uid, Timestamp(2_000)).await.unwrap());
	assert_eq!(
		store.api_key_by_prefix("aaaaaaaa").await.unwrap().unwrap().revoked_at,
		Some(Timestamp(1_000))
	);
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
	let (account, _) = store.create_account(&new_account("erased@e.st"), &[], None).await.unwrap();
	store.anonymize_account(account.id, Timestamp(1_000), &ERASURE).await.unwrap();

	for status in [AccountStatus::Active, AccountStatus::Pending, AccountStatus::Suspended] {
		let err = store.set_account_status(account.id, status).await.unwrap_err();
		assert_eq!(err.parts().1, "E-AUTH-ANONYMIZED", "{status:?} was accepted");
	}
	// The guard is narrow: an ordinary account still moves freely, and setting ANONYMIZED on
	// an already-anonymized one is a no-op rather than an error — `anonymize_account` is
	// idempotent and must stay so.
	let (live, _) = store.create_account(&new_account("live@e.st"), &[], None).await.unwrap();
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
	let (account, _tenant) =
		store.create_account(&new_account("suspend@e.st"), &[], None).await.unwrap();
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
	let (account, _tenant) =
		store.create_account(&new_account("consent@e.st"), &[], None).await.unwrap();

	let grant = |version: &'static str| NewConsent {
		account_id: account.id,
		tenant_id: None,
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

// vim: ts=4
