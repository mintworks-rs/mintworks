//! The persistence contract for `saas-auth`, and the row types it moves.
//!
//! Implemented for `SqliteStore` in `adapters/store-adapter-sqlite/src/auth.rs`. Public
//! identifiers (`AccountId`, `TenantId`, `ApiKeyId`) are the `uid` columns; the `i64`
//! arguments below are internal primary keys and never appear in a URL.

use async_trait::async_trait;
use saas_core::prelude::*;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- enums

/// `accounts.status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum AccountStatus {
	Pending,
	Active,
	Suspended,
	Anonymized,
}

/// `tenants.kind` — a personal tenant is created with its account; an organisation is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TenantKind {
	#[serde(rename = "P")]
	Personal,
	#[serde(rename = "O")]
	Organisation,
}

/// `tenants.status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TenantStatus {
	Active,
	Suspended,
}

/// `memberships.role`. Re-read from the database on privileged routes rather than trusted
/// from the access token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Role {
	Member,
	Admin,
	Owner,
}

/// `legal_docs.kind` and `consents.kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LegalKind {
	#[serde(rename = "TOS")]
	Tos,
	#[serde(rename = "PRIVACY")]
	Privacy,
	#[serde(rename = "EINVOICE")]
	EInvoice,
	#[serde(rename = "WITHDRAWAL_WAIVER")]
	WithdrawalWaiver,
}

saas_core::str_enum!(AccountStatus {
	Pending => "PENDING",
	Active => "ACTIVE",
	Suspended => "SUSPENDED",
	Anonymized => "ANONYMIZED",
});
saas_core::str_enum!(TenantKind { Personal => "P", Organisation => "O" });
saas_core::str_enum!(TenantStatus { Active => "ACTIVE", Suspended => "SUSPENDED" });
saas_core::str_enum!(Role { Member => "MEMBER", Admin => "ADMIN", Owner => "OWNER" });
saas_core::str_enum!(LegalKind {
	Tos => "TOS",
	Privacy => "PRIVACY",
	EInvoice => "EINVOICE",
	WithdrawalWaiver => "WITHDRAWAL_WAIVER",
});

// ---------------------------------------------------------------- rows

/// One row of `accounts`.
#[derive(Clone, Debug)]
pub struct Account {
	pub id: i64,
	pub uid: AccountId,
	pub email: String,
	/// argon2id; `None` for an invited account that has not set a password yet.
	pub pwd_hash: Option<String>,
	pub name: Option<String>,
	pub locale: String,
	pub status: AccountStatus,
	pub token_epoch: i64,
	pub is_operator: bool,
	pub failed_logins: i64,
	pub locked_until: Option<Timestamp>,
	pub activated_at: Option<Timestamp>,
	pub last_login_at: Option<Timestamp>,
	pub anonymized_at: Option<Timestamp>,
	pub created_at: Timestamp,
}

/// Registration input. The personal tenant is created in the same transaction.
#[derive(Clone, Debug)]
pub struct NewAccount {
	/// Lowercased and trimmed by the caller before it gets here.
	pub email: String,
	pub pwd_hash: Option<String>,
	pub name: Option<String>,
	pub locale: String,
	/// Name of the personal tenant created alongside the account.
	pub tenant_name: String,
}

/// One row of `tenants`.
#[derive(Clone, Debug)]
pub struct Tenant {
	pub id: i64,
	pub uid: TenantId,
	pub kind: TenantKind,
	pub name: String,
	pub owner_account_id: i64,
	/// `None` falls back to the `currency.base` setting.
	pub billing_currency: Option<CurrencyCode>,
	pub status: TenantStatus,
	pub created_at: Timestamp,
}

/// A tenant as seen from one account's membership in it — the login response body.
#[derive(Clone, Debug)]
pub struct AccountTenant {
	pub uid: TenantId,
	pub kind: TenantKind,
	pub name: String,
	pub status: TenantStatus,
	pub role: Role,
	/// `NULL` while an invitation is outstanding. [`crate::token::pick_tenant`] skips those:
	/// an invite nobody accepted must not become the invitee's default tenant.
	pub accepted_at: Option<Timestamp>,
}

/// A membership as seen from the tenant's side — the member list.
#[derive(Clone, Debug)]
pub struct Member {
	pub account_uid: AccountId,
	/// `None` until the invitation is accepted — see [`AuthStore::members`].
	pub email: Option<String>,
	pub name: Option<String>,
	pub role: Role,
	/// The member's `accounts.status`, `None` while the membership is pending.
	pub status: Option<AccountStatus>,
	/// `memberships.accepted_at IS NOT NULL`. A pending row is an invitation nobody has
	/// answered, and every column that would describe the invitee is withheld.
	pub accepted: bool,
	pub created_at: Timestamp,
}

/// One row of `api_keys`. `key_hash` is the argon2id of the full key; only `prefix` is
/// ever looked up.
#[derive(Clone, Debug)]
pub struct ApiKey {
	pub id: i64,
	pub uid: ApiKeyId,
	pub tenant_id: i64,
	pub account_id: i64,
	pub name: String,
	pub prefix: String,
	pub key_hash: String,
	/// JSON array of route scopes.
	pub scopes: String,
	pub last_used_at: Option<Timestamp>,
	pub expires_at: Option<Timestamp>,
	pub revoked_at: Option<Timestamp>,
	pub created_at: Timestamp,
}

/// API key input; the plaintext key never reaches the store.
#[derive(Clone, Debug)]
pub struct NewApiKey {
	pub tenant_id: i64,
	pub account_id: i64,
	pub name: String,
	pub prefix: String,
	pub key_hash: String,
	pub scopes: String,
	pub expires_at: Option<Timestamp>,
}

/// One row of `totp_credentials`. The secret is AES-256-GCM under `HKDF(MASTER_KEY, 'totp')`;
/// the store moves the ciphertext and never sees the key.
#[derive(Clone, Debug)]
pub struct TotpCredential {
	pub account_id: i64,
	pub secret_nonce: Vec<u8>,
	pub secret_enc: Vec<u8>,
	pub digits: i64,
	pub period: i64,
	/// JSON array of argon2id hashes; a used code is removed from the array.
	pub recovery_hashes: String,
	/// Replay guard: the last accepted time step.
	pub last_used_step: Option<i64>,
	/// `None` while enrolment is begun but not yet verified.
	pub confirmed_at: Option<Timestamp>,
	pub created_at: Timestamp,
}

/// Enrolment input; replaces any unconfirmed credential for the account.
#[derive(Clone, Debug)]
pub struct NewTotpCredential {
	pub account_id: i64,
	pub secret_nonce: Vec<u8>,
	pub secret_enc: Vec<u8>,
	pub digits: i64,
	pub period: i64,
	pub recovery_hashes: String,
}

/// One version of one consentable text, in one locale.
#[derive(Clone, Debug)]
pub struct LegalDoc {
	pub id: i64,
	pub kind: LegalKind,
	pub locale: String,
	pub version: String,
	pub title: String,
	/// Markdown, verbatim as presented.
	pub body: String,
	/// Hex SHA-256 of `body`.
	pub sha256: String,
	pub effective_from: Timestamp,
}

/// A legal document to publish. `sha256` is computed by the caller over `body`.
#[derive(Clone, Debug)]
pub struct NewLegalDoc {
	pub kind: LegalKind,
	pub locale: String,
	pub version: String,
	pub title: String,
	pub body: String,
	pub sha256: String,
	pub effective_from: Timestamp,
}

/// One row of `consents`. Evidence for a legal claim: not erased by a GDPR request.
#[derive(Clone, Debug)]
pub struct Consent {
	pub id: i64,
	pub kind: LegalKind,
	/// `None` for account-level consent.
	pub tenant_uid: Option<TenantId>,
	pub doc_version: String,
	pub doc_sha256: String,
	pub granted: bool,
	pub at: Timestamp,
	pub withdrawn_at: Option<Timestamp>,
}

/// An acceptance to record. `doc_version` and `doc_sha256` are copied, not referenced, so
/// they survive a purge of `legal_docs`.
#[derive(Clone, Debug)]
pub struct NewConsent {
	pub account_id: i64,
	/// `None` for account-level consent.
	pub tenant_id: Option<i64>,
	pub kind: LegalKind,
	pub legal_doc_id: Option<i64>,
	pub doc_version: String,
	pub doc_sha256: String,
	pub granted: bool,
	pub ip: Option<String>,
	pub user_agent: Option<String>,
}

// ---------------------------------------------------------------- trait

/// Everything `saas-auth` needs from a database.
///
/// Errors are `saas-core`'s: a unique-constraint violation surfaces as [`Error::Conflict`],
/// anything else as `Error::Internal`. Methods that "consume" a row report whether they matched,
/// so a caller can distinguish a spent token from an unknown one without a second query.
#[async_trait]
pub trait AuthStore: Send + Sync + 'static {
	// -- accounts

	/// Creates the account, its personal tenant, the owning membership **and `consents`**
	/// in one transaction. [`Error::Conflict`] if the email is taken.
	///
	/// The consents belong in here rather than in a follow-up call: registration is one
	/// service-level operation, and an account committed without its ToS/privacy rows is blocked by
	/// `consent::gate` on every gated route with no way back. Each entry's `account_id` is ignored
	/// — the caller cannot know it yet — and filled from the inserted row. Pass an empty slice for
	/// an account that consents to nothing yet, such as an invitee.
	///
	/// `join` is a tenant to join at creation, written in the same transaction — the invite
	/// path, so a committed account always has the membership it was created for. Written
	/// afterwards, a `SQLITE_BUSY` past the writer's 5 s timeout left the invited address
	/// permanently registered `PENDING` with a personal tenant and no membership anywhere,
	/// and every later invite of that address took the `already_registered` path.
	async fn create_account(
		&self,
		new: &NewAccount,
		consents: &[NewConsent],
		join: Option<(i64, Role)>,
	) -> ClResult<(Account, Tenant)>;

	async fn account_by_email(&self, email: &str) -> ClResult<Option<Account>>;

	async fn account_by_uid(&self, uid: &AccountId) -> ClResult<Option<Account>>;

	async fn account_by_id(&self, id: i64) -> ClResult<Option<Account>>;

	/// `PENDING` -> `ACTIVE`, stamping `activated_at` and, when `pwd_hash` is `Some`,
	/// setting the password in the same statement — an invited account has none until it
	/// activates, and a half-landed activation would leave an account that can never log
	/// in. `false` if the account was not pending, which is how a replayed activation is
	/// detected.
	async fn activate_account(
		&self,
		id: i64,
		pwd_hash: Option<&str>,
		at: Timestamp,
	) -> ClResult<bool>;

	/// Sets the password hash and bumps `token_epoch`, invalidating live tokens.
	///
	/// Compare-and-swap on `token_epoch`: the write lands only if the account is still at
	/// `expected_epoch` — the value the caller verified its reset token against — and reports
	/// whether it won. Unconditional, two concurrent redemptions of the same mailed token
	/// both read the account first and both succeeded, so the token was not single-use.
	async fn set_password(&self, id: i64, expected_epoch: i64, pwd_hash: &str) -> ClResult<bool>;

	/// Sets the account's status.
	///
	/// **`ANONYMIZED` is terminal.** An anonymized account accepts no other status; an attempt
	/// to move one back is `E-AUTH-ANONYMIZED`/409, and any other missing account is silently
	/// nothing to do, as this method's `ClResult<()>` has always reported it. GDPR erasure has
	/// to be irreversible, and `anonymize_account` leaves `email = 'anonymized+…@invalid'` with
	/// `pwd_hash = NULL` — a row `auth_mw::account_for_token` would accept again the moment it
	/// were ACTIVE.
	///
	/// This doc is the contract, not the schema: a business rule the application enforces does
	/// not belong in a trigger too, and a trigger defends against an accident, never an
	/// adversary. Mirrors
	/// [`crate::store::AuthStore::activate_account`]'s `PENDING` precondition and
	/// `InvoiceStore::mark_paid`/`mark_stornoed`, which name their legal predecessor the same
	/// way. `Auth::erase_account` refuses a second erasure before it gets here.
	///
	/// Moving an account to `SUSPENDED` also bumps `token_epoch`, **in the same transaction**:
	/// a suspension that did not revoke is the failure this method exists to prevent. Any
	/// other status leaves the epoch alone.
	async fn set_account_status(&self, id: i64, status: AccountStatus) -> ClResult<()>;

	/// `token_epoch += 1`: the revocation lever. Auth is stateless JWT with no session table
	/// and no denylist, so this is the only way to invalidate an issued token — and it signs
	/// every device out, not one. Silently nothing to do for a missing account, as
	/// [`AuthStore::set_account_status`] is.
	async fn bump_token_epoch(&self, id: i64) -> ClResult<()>;

	/// Counts one failed attempt. One `UPDATE`, no policy.
	///
	/// No lockout policy in SQL: per-account brute force is `ratelimit`'s `login.email` scope,
	/// and this counter is a signal, not a gate.
	///
	/// **The write is as load-bearing as the count.** `saas_auth::login::record_failure` calls
	/// this with `NO_ACCOUNT` on the unknown-address branch precisely so that branch costs the
	/// same writer round trip as the real one; without it the route is an account-existence
	/// oracle. An implementation must not skip the statement when `id` matches no row.
	async fn record_login_failure(&self, id: i64) -> ClResult<()>;

	/// Clears `failed_logins` and `locked_until`, stamps `last_login_at`.
	async fn record_login_success(&self, id: i64, at: Timestamp) -> ClResult<()>;

	// -- tenants

	/// Creates a tenant, its `billing_currency` and its owner membership in one transaction.
	///
	/// The currency is a parameter rather than a follow-up `update_tenant`: as two statements,
	/// a failure between them committed a tenant carrying the wrong currency while the caller
	/// saw a 500. `None` means the `currency.base` setting.
	async fn create_tenant(
		&self,
		kind: TenantKind,
		name: &str,
		owner_account_id: i64,
		billing_currency: Option<&CurrencyCode>,
	) -> ClResult<Tenant>;

	async fn tenant_by_uid(&self, uid: &TenantId) -> ClResult<Option<Tenant>>;

	async fn tenant_by_id(&self, id: i64) -> ClResult<Option<Tenant>>;

	async fn update_tenant(
		&self,
		id: i64,
		name: Option<&str>,
		billing_currency: Patch<CurrencyCode>,
		status: Option<TenantStatus>,
	) -> ClResult<()>;

	/// Hand `tenant_id` from `from` to `to`: demote `from` to `ADMIN`, promote `to` to `OWNER`,
	/// and move `tenants.owner_account_id`. One transaction, because two of the three alone
	/// leaves an organisation nobody can administer.
	///
	/// `false` — not an error — when `to` has no **accepted** membership on the tenant, or when
	/// `from` is not its current owner. The predicate is re-run inside the write transaction the
	/// way [`Self::anonymize_account`] re-runs its own: the service's check reads the reader pool.
	async fn transfer_tenant_ownership(&self, tenant_id: i64, from: i64, to: i64)
	-> ClResult<bool>;

	/// Delete an organisation. `memberships`, `api_keys` and `billing_parties` cascade.
	///
	/// `false` when any *other* accepted membership remains, or when the tenant still owns rows
	/// that must be retained — which is the adapter's call, because it owns the DDL: `invoices`,
	/// under the eight-year Hungarian retention obligation, and tenant-scoped `consents`, which
	/// are evidence. Both are plain `REFERENCES tenants(id)` with no cascade, so the refusal is
	/// deliberate rather than an FK error.
	async fn delete_tenant(&self, tenant_id: i64) -> ClResult<bool>;

	/// Every tenant the account is a member of, with its role there.
	async fn tenants_for_account(&self, account_id: i64) -> ClResult<Vec<AccountTenant>>;

	/// The **organisation** tenants the account owns — `owner_account_id = account_id AND
	/// kind != 'P'`. Erasure is refused while this is non-empty: `anonymize_account` scrubs
	/// only the personal tenant, so an org would keep pointing at the erased row, and
	/// `remove_member` refuses to remove an `OWNER` while `set_member_role` refuses to
	/// assign one — the organisation would be permanently un-administrable.
	async fn owned_org_tenants(&self, account_id: i64) -> ClResult<Vec<Tenant>>;

	/// Whether `code` is an enabled currency. `currencies` belongs to `saas-invoice`; a
	/// deployment without that crate has no table and every code passes.
	async fn currency_enabled(&self, code: &CurrencyCode) -> ClResult<bool>;

	// -- memberships

	/// The privileged re-check: role read from the database, never from the token.
	///
	/// Deliberately **unfiltered** by `accepted_at`: the member-management routes must still
	/// see a pending invitation, or an admin loses the ability to re-role or remove one. The
	/// authorization-path counterpart is [`AuthStore::accepted_membership_role`].
	async fn membership_role(&self, tenant_id: i64, account_id: i64) -> ClResult<Option<Role>>;

	/// [`AuthStore::membership_role`] restricted to an accepted membership — what an
	/// authorization check must ask, so a pending invitee cannot act inside the tenant.
	async fn accepted_membership_role(
		&self,
		tenant_id: i64,
		account_id: i64,
	) -> ClResult<Option<Role>>;

	/// Marks an invitation accepted. Switching into a tenant is the explicit act that
	/// accepts it (`token::pick_tenant` skips unaccepted ones), so this is idempotent and
	/// a no-op on a membership that is already accepted.
	async fn accept_membership(
		&self,
		tenant_id: i64,
		account_id: i64,
		at: Timestamp,
	) -> ClResult<()>;

	/// `false` if the row exists and is the `OWNER`'s: the precondition lives in the statement
	/// because the service's own check reads the reader pool, where a concurrent
	/// `transfer_tenant_ownership` is invisible. Inserting a new `OWNER` row is unaffected.
	async fn put_membership(&self, tenant_id: i64, account_id: i64, role: Role) -> ClResult<bool>;

	/// `false` if there was no such membership, or it is the `OWNER`'s — the statement carries
	/// that precondition for the same reason [`AuthStore::put_membership`] does.
	/// Deliberately does **not** touch
	/// `token_epoch`: that is account-wide, and removing one membership must not sign the
	/// account out of every other tenant it belongs to. `auth_mw`'s tenant join requires a
	/// live membership row, so the deleted row is what ends this tenant's access.
	async fn remove_membership(&self, tenant_id: i64, account_id: i64) -> ClResult<bool>;

	/// The membership row's own `created_at`. Read back rather than assumed: the route used to
	/// report `Timestamp::now()` for a membership that may be years old.
	async fn membership_created_at(
		&self,
		tenant_id: i64,
		account_id: i64,
	) -> ClResult<Option<Timestamp>>;

	/// Every membership of `tenant_id`, with `email`, `name` and `status` withheld for the
	/// pending ones: an invitation must not answer whether the address is registered or who
	/// owns it. Capped at `limit`, which the handle sets: a tenant admin grows this table by
	/// inviting, and the statement had no `LIMIT` at all.
	async fn members(&self, tenant_id: i64, limit: i64) -> ClResult<Vec<Member>>;

	// -- api keys

	async fn create_api_key(&self, new: &NewApiKey) -> ClResult<ApiKey>;

	/// The only lookup path: `prefix` is the first 8 characters of the presented key.
	/// Revocation and expiry are checked by the caller against the returned row.
	async fn api_key_by_prefix(&self, prefix: &str) -> ClResult<Option<ApiKey>>;

	async fn api_keys_for_tenant(&self, tenant_id: i64) -> ClResult<Vec<ApiKey>>;

	async fn touch_api_key(&self, id: i64, at: Timestamp) -> ClResult<()>;

	/// `false` if the key does not exist, belongs to another tenant, or was already revoked.
	///
	/// `tenant_id` is part of the signature because a `key_<ULID>` arrives from a request
	/// body: without it any tenant could revoke any other tenant's key. Collapsing the
	/// foreign-tenant case into the same `false` is also what keeps it an `E-CORE-NOTFOUND`
	/// rather than a 403.
	async fn revoke_api_key(&self, tenant_id: i64, uid: &ApiKeyId, at: Timestamp)
	-> ClResult<bool>;

	// -- totp

	/// Upsert: a re-enrolment replaces the stored credential and its recovery codes. `false`
	/// when a **confirmed** credential is already there and nothing was written.
	///
	/// Consume-style, like [`Self::confirm_totp`] and [`Self::advance_totp_step`]: the
	/// precondition `confirmed_at IS NULL` travels with the write rather than being read
	/// first, so a `confirm_totp` racing an enrolment cannot be silently wiped — which left
	/// 2FA off and the user holding recovery codes that matched nothing.
	async fn put_totp(&self, new: &NewTotpCredential) -> ClResult<bool>;

	async fn totp_by_account(&self, account_id: i64) -> ClResult<Option<TotpCredential>>;

	/// Stamps `confirmed_at` **and** writes the recovery-code array, completing enrolment.
	///
	/// One statement, deliberately. Arming the factor and storing its recovery codes used to
	/// be two writes with N argon2 hashes between them: a failure in the gap left
	/// `confirmed_at` set with an empty `recovery_hashes` and the plaintext codes never
	/// returned, and `enrol` refuses an already-confirmed credential — an unrecoverable
	/// lockout with no route out.
	/// Carries its own precondition: the `UPDATE` matches only while `confirmed_at IS NULL`,
	/// and `false` means it did not — the factor was already armed, by an earlier request or
	/// by one racing this one. A separate read could not say that: two `POST
	/// /api/auth/totp/verify` with codes from adjacent skew steps both passed
	/// `advance_totp_step` and both returned a recovery-code set, and the user kept the first
	/// while the database stored the second.
	async fn confirm_totp(&self, account_id: i64, at: Timestamp, hashes: &str) -> ClResult<bool>;

	/// Records the accepted time step. `false` if `step` is not greater than the stored
	/// one, which is the replay rejection.
	async fn advance_totp_step(&self, account_id: i64, step: i64) -> ClResult<bool>;

	/// Compare-and-swap on the recovery-code array: writes `hashes` only if the stored
	/// value is still `expected`, and reports whether it won. A read-modify-write would
	/// let two requests presenting the same code both succeed, and two requests
	/// presenting different codes clobber each other back into a spent code.
	async fn swap_totp_recovery(
		&self,
		account_id: i64,
		expected: &str,
		hashes: &str,
	) -> ClResult<bool>;

	/// `false` if there was no credential.
	async fn delete_totp(&self, account_id: i64) -> ClResult<bool>;

	// -- legal docs and consent

	async fn insert_legal_doc(&self, new: &NewLegalDoc) -> ClResult<i64>;

	/// The newest version of `kind` in `locale` whose `effective_from` has passed, falling back
	/// to any published locale — a kind published anywhere has to gate, or an account whose
	/// locale lacks a translation sails past the consent wall having accepted nothing.
	async fn current_legal_doc(
		&self,
		kind: LegalKind,
		locale: &str,
		now: Timestamp,
	) -> ClResult<Option<LegalDoc>>;

	async fn record_consent(&self, new: &NewConsent, at: Timestamp) -> ClResult<i64>;

	/// The most recent consent of that kind **in that scope**, withdrawn or not. `tenant_id`
	/// is `None` for an account-level consent and matches NULL, so the two scopes are
	/// separate rows rather than one.
	///
	/// The scope is part of the key because `consents.tenant_id` is populated
	/// (`Auth::record_consent` validates a `tenantUid` against an accepted membership) and
	/// was then never read: the latest row *per kind* across all tenants was the only one
	/// `GET /api/consents` and `DELETE /api/consents/{kind}` could reach, so a grant for
	/// tenant A became invisible and unwithdrawable the moment one was made for tenant B.
	async fn latest_consent(
		&self,
		account_id: i64,
		kind: LegalKind,
		tenant_id: Option<i64>,
	) -> ClResult<Option<Consent>>;

	/// The most recent consent per `(kind, tenant_id)` for the account — one row per scope,
	/// not one per kind. Feeds `GET /api/consents`; see [`Self::latest_consent`].
	async fn list_consents(&self, account_id: i64) -> ClResult<Vec<Consent>>;

	/// `false` if the consent does not exist or was already withdrawn.
	async fn withdraw_consent(&self, id: i64, at: Timestamp) -> ClResult<bool>;

	// -- gdpr

	/// The rows named by `sections`, one JSON array each, in the same order —
	/// [`crate::gdpr::EXPORT`] names them and [`crate::gdpr::document`] assembles the
	/// `GET /api/account/export` document from the result.
	///
	/// All of them must come from **one snapshot**: a write landing mid-export tears the
	/// document, and this is evidence in a subject access request. A section whose table the
	/// deployment does not have comes back as `[]`.
	async fn export_account(
		&self,
		account_id: i64,
		sections: &[ExportSection],
	) -> ClResult<Vec<serde_json::Value>>;

	/// Erasure by the closed column allowlist in `plan` ([`crate::gdpr::ERASURE`]), in one
	/// transaction. It must touch no column `plan` does not name, beyond the `accounts`
	/// mechanics [`ErasurePlan`] documents.
	///
	/// Returns `false`, having written nothing, when the account still owns an organisation
	/// tenant at commit time ([`AuthStore::owned_org_tenants`]). The service pre-checks that on
	/// the reader for the message; this is the guarantee, because a `POST /api/tenants` landing
	/// between the two anonymized the owner of a live organisation.
	async fn anonymize_account(
		&self,
		account_id: i64,
		at: Timestamp,
		plan: &ErasurePlan,
	) -> ClResult<bool>;
}

// -- gdpr allowlist

/// How an export section reaches the subject's rows. These are the three relationships this
/// schema actually has, not a query language — widen it only when a new table can be reached
/// by none of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportScope {
	/// `accounts` itself: the row with this id.
	Account,
	/// An `account_id` column holding the account's id.
	AccountId,
	/// Every tenant the account is a member of, organisation included — which organisations a
	/// person belongs to *is* their personal data. An owner holds an `OWNER` membership row
	/// (`create_tenant` writes it in the same transaction), so this covers ownership too.
	MemberTenant,
	/// A `tenant_id` under the account's **personal** tenant only. An organisation this
	/// account merely owns holds other people's rows, which a subject access request may
	/// not hand over.
	PersonalTenant,
	/// An `invoice_id` of an invoice under the account's personal tenant.
	PersonalTenantInvoice,
}

/// One section of the export document: its JSON key, the table it reads, how that table is
/// scoped to the subject, and the closed column allowlist. Empty `columns` exports `[]`.
///
/// `id` is never exported; an INTEGER `*_id` column is exported as the referenced row's
/// public `uid` under a `…Uid` key, or dropped when that table has none. `at` and `*_at` are
/// rendered ISO-8601 UTC, and every key is camelCase.
#[derive(Debug, Clone, Copy)]
pub struct ExportSection {
	pub key: &'static str,
	pub table: &'static str,
	pub scope: ExportScope,
	pub columns: &'static [&'static str],
	/// Columns whose stored integer is scaled, and how. A column here must also be in
	/// `columns`; anything absent exports verbatim.
	pub scaled: &'static [(&'static str, Scale)],
	/// Columns rendered NULL unless the row satisfies the predicate. The predicate is a
	/// `&'static` SQL fragment over the section's own table, never a request value.
	pub mask: &'static [(&'static str, &'static str)],
}

/// How a stored integer is rendered in the export. Everything not listed exports verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scale {
	/// [`Money`] minor units, rendered `{"amount": "…", "currency": "…"}` with the row's own
	/// `currency` value, or a bare decimal string when the row carries none.
	Money,
	/// [`Money`] minor units that are statutory HUF regardless of the invoice's currency.
	MoneyHuf,
	/// [`Qty`], scaled 1e6.
	Qty,
}

/// A column an erasure clears, and the literal it is set to. `None` is SQL NULL.
pub type ErasedCol = (&'static str, Option<&'static str>);

/// The closed column allowlist an erasure may touch. Defined once, in [`crate::gdpr::ERASURE`].
///
/// Three things are the store's own mechanics rather than allowlist entries, because they
/// are not a choice about what counts as personal data: `accounts.email` is replaced by a
/// placeholder that keeps the UNIQUE index satisfied, `status`/`anonymized_at`/`token_epoch`
/// record the erasure and kill every live token, and every `api_keys` row the account holds is
/// revoked — scoped by `account_id`, whatever tenant the key is scoped to, because a key
/// belongs to the person and a member's organisation-scoped keys outlived their own erasure
/// under a personal-tenant scope.
#[derive(Debug, Clone, Copy)]
pub struct ErasurePlan {
	/// `accounts` columns cleared, for the erased row.
	pub accounts: &'static [ErasedCol],
	/// `tenants` columns cleared, `kind = 'P'` and owned by the account.
	pub tenants: &'static [ErasedCol],
	/// `billing_parties` columns cleared, `kind = 'P'` under the personal tenant.
	pub billing_parties: &'static [ErasedCol],
	/// Tables whose rows keyed by `account_id` are deleted outright.
	pub delete_by_account: &'static [&'static str],
	/// `jobs.kind`s whose payload is blanked when it is addressed to the erased address.
	pub blank_job_kinds: &'static [&'static str],
}

// vim: ts=4
