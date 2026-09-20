//! The persistence this crate needs: the `payments` row, its `payment_allocations` rows, and
//! the `invoices.paid_amount` cache they maintain.
//!
//! A caller cannot hand a transaction to a store method, so anything that must be
//! all-or-nothing is **one** method here rather than several the service composes.
//! [`BillingStore::settle`] is the whole of that: payment row, allocation row and invoice
//! cache in one call.
//!
//! **Every status write is guarded by the current status.** The callback endpoint is public
//! and the gateway retries, so a replayed ping must be a no-op; the guard is what makes it
//! one, and `false` is how the adapter says the write did not apply.

use std::sync::Arc;

use async_trait::async_trait;
use saas_core::prelude::*;

use crate::provider::PaymentState;

/// A row of `payments`.
#[derive(Debug, Clone)]
pub struct Payment {
	pub id: i64,
	pub uid: PaymentId,
	pub org_id: i64,
	/// Open string, deliberately: `'BARION'`, `'TRANSFER'`, `'MANUAL'`, or a consumer's own.
	/// The column carries no `CHECK`, so a credit system records its own kind with no schema
	/// change.
	pub kind: String,
	/// [`crate::PaymentProvider::id`] when a gateway is behind it; `None` for a transfer or a
	/// manual entry.
	pub provider: Option<String>,
	pub provider_ref: Option<String>,
	/// Where the gateway wants the browser sent. Stored rather than handed back once, so a
	/// customer who navigated away resumes the payment they already have — a retry under a
	/// spent `request_id` opens no second payment and so has no fresh URL of its own.
	pub redirect_url: Option<String>,
	pub request_id: Option<String>,
	pub status: PaymentState,
	pub amount: Money,
	pub currency: CurrencyCode,
	pub refunded_amount: Money,
	pub received_at: Option<Timestamp>,
	/// Bank reference or remittance note, for matching a transfer by hand.
	pub ext_ref: Option<String>,
	pub note: Option<String>,
	/// `accounts.id` for a manual entry. No FK: it survives anonymization.
	pub created_by: Option<i64>,
	pub created_at: Timestamp,
	pub updated_at: Timestamp,
}

/// What the caller supplies for a new `payments` row. `uid`, the timestamps and
/// `refunded_amount` are the store's.
#[derive(Debug, Clone)]
pub struct NewPayment {
	pub org_id: i64,
	pub kind: String,
	pub provider: Option<String>,
	pub provider_ref: Option<String>,
	pub request_id: Option<String>,
	pub status: PaymentState,
	pub amount: Money,
	pub currency: CurrencyCode,
	pub ext_ref: Option<String>,
	pub note: Option<String>,
	pub created_by: Option<i64>,
	/// The invoice this payment was opened for, when it was opened for one.
	/// [`BillingStore::create_payment`] writes its `payment_allocations` row with `amount = 0`
	/// in the same transaction: `payments` has no invoice column, so that row *is* the link a
	/// callback follows back from a `provider_ref`, and [`BillingStore::settle`] adds to it
	/// rather than inserting a second.
	pub invoice_id: Option<i64>,
}

/// A row of `payment_allocations`: how much of one payment settled one invoice.
#[derive(Debug, Clone)]
pub struct PaymentAllocation {
	pub payment_id: i64,
	pub invoice_id: i64,
	pub invoice_uid: InvoiceId,
	/// `NULL` on a draft, which has no number until it is issued.
	pub invoice_number: Option<String>,
	/// Invoice-currency minor units. Negative on the pair reverses, which is how a refund is
	/// recorded.
	pub amount: Money,
	pub allocated_at: Timestamp,
	pub allocated_by: Option<i64>,
}

/// The arguments of [`BillingStore::settle`]. A struct because the call is one transaction and
/// so cannot be split into smaller ones.
#[derive(Debug, Clone)]
pub struct Settlement {
	pub payment_id: i64,
	/// The statuses the payment must currently be in. Anything else and nothing is written —
	/// this is the replay guard. Empty matches nothing and answers `false`; the adapter must
	/// not render it as an unguarded `UPDATE`.
	pub from: Vec<PaymentState>,
	pub to: PaymentState,
	pub invoice_id: i64,
	/// Invoice-currency minor units; negative reverses an earlier allocation.
	pub amount: Money,
	pub at: Timestamp,
	/// `accounts.id` when a person allocated it by hand.
	pub allocated_by: Option<i64>,
	/// The most this payment's allocations may sum to, checked **inside** the transaction.
	/// `None` imposes none, for a caller that allocates the payment's own amount and so cannot
	/// exceed it. A Rust-side check on a reader connection is a read-then-write: two concurrent
	/// allocations of the whole payment both saw zero and both committed.
	pub ceiling: Option<Money>,
}

/// The arguments of [`BillingStore::record_refund`]. A struct for the reason [`Settlement`]
/// is one: the `refunded_amount` bump and the allocation it reverses are a single
/// transaction and cannot be split into two calls.
#[derive(Debug, Clone)]
pub struct RefundRecord {
	pub payment_id: i64,
	/// The statuses the payment must currently be in — the replay guard, as in [`Settlement`].
	/// A refund leaves a *terminal* status, which `crate::allocate`'s live-status walk cannot
	/// produce, so this list is always the caller's own. Empty matches nothing and answers
	/// `false`; the adapter must not render it as an unguarded `UPDATE`.
	pub from: Vec<PaymentState>,
	pub to: PaymentState,
	/// Positive minor units: what is being given back. The store adds it to `refunded_amount`.
	pub amount: Money,
	/// The `refunded_amount` this refund was computed against, re-checked inside the
	/// transaction. `refund::refund` derives the gateway's idempotency key from it, so two
	/// concurrent refunds send one key, the gateway pays once, and without this both record it.
	pub expect_refunded: Money,
	/// How much of `amount` comes back out of the allocation against `invoice_id`, as a
	/// positive figure the store negates. A refund eats the payment's unallocated part first,
	/// so refunding an overpayment leaves the settled invoice alone and this is zero.
	pub reverse: Money,
	/// The invoice whose allocation is reversed, when this payment settled one. `None`
	/// whenever `reverse` is zero.
	pub invoice_id: Option<i64>,
	pub at: Timestamp,
	/// `accounts.id` of the operator.
	pub by: Option<i64>,
}

/// The filters `GET /api/payments` accepts, all optional and ANDed.
#[derive(Debug, Default, Clone)]
pub struct PaymentFilter<'a> {
	/// Exclusive, and a **uid**: `payments.id` is global rather than per org, so putting it
	/// on the wire handed any org a cross-org row-volume oracle (`ids.rs`). Resolved
	/// inside the query, so an unknown or another org's uid is an empty page and never an
	/// error — another org's cursor is not a cursor.
	pub before: Option<&'a PaymentId>,
	pub limit: i64,
	pub status: Option<PaymentState>,
	pub kind: Option<&'a str>,
	pub provider: Option<&'a str>,
	/// Matched through `payment_allocations`, since `payments` has no invoice column.
	pub invoice_uid: Option<&'a InvoiceId>,
	pub received_from: Option<Timestamp>,
	pub received_to: Option<Timestamp>,
}

/// One overdue invoice, as both the dunning sweep and the admin aging list see it.
#[derive(Debug, Clone)]
pub struct OverdueInvoice {
	pub invoice_id: i64,
	pub invoice_uid: InvoiceId,
	pub org_id: i64,
	pub number: String,
	/// `'YYYY-MM-DD'`.
	pub due_date: String,
	pub days_overdue: i64,
	/// `gross - paid_amount`, in the invoice's own currency.
	pub outstanding: Money,
	pub currency: CurrencyCode,
	pub buyer_name: String,
	/// The frozen snapshot's country, which is the only locale signal an issued invoice has.
	pub buyer_country: Option<String>,
	/// `billing_parties.email`, nullable: an aging row need not be mailable.
	pub buyer_email: Option<String>,
}

#[async_trait]
pub trait BillingStore: Send + Sync + 'static {
	/// Inserts the row and mints its `uid`.
	///
	/// `payments.request_id` is `UNIQUE (org_id, request_id)`, so a second create under an
	/// idempotency key that org has already spent answers `E-CORE-CONFLICT` rather than
	/// opening a second payment — which is what makes `POST /api/invoices/{uid}/pay` idempotent
	/// without a read-then-write race. Per org, not global: `request_id` is client text, so a
	/// global key let one org squat another's natural keys and probe for them.
	async fn create_payment(&self, new: &NewPayment) -> ClResult<Payment>;

	async fn payment(&self, id: i64) -> ClResult<Option<Payment>>;

	/// `org_id` `None` is every org, and is for the operator paths, which are gated by
	/// `require_operator` instead. An org-scoped caller passes `Some`, so another org's
	/// `uid` answers `None`, which the caller turns into `E-CORE-NOTFOUND` and never a 403.
	async fn payment_by_uid(
		&self,
		org_id: Option<i64>,
		uid: &PaymentId,
	) -> ClResult<Option<Payment>>;

	/// The callback's lookup. `idx_payment_provider_ref` is unique over
	/// `(provider, provider_ref)`, so a duplicated ping finds the one row and the guarded
	/// transitions above turn the second into a no-op.
	async fn payment_by_provider_ref(
		&self,
		provider: &str,
		provider_ref: &str,
	) -> ClResult<Option<Payment>>;

	/// The idempotency lookup, for answering a retried start with the payment it already made.
	///
	/// **Org-scoped**: `request_id` is client text, not a server-minted uid, so a global
	/// lookup answered one org's start with another org's payment. The uniqueness is
	/// per org too, so the same key in two orgs is two independent payments.
	async fn payment_by_request_id(
		&self,
		org_id: i64,
		request_id: &str,
	) -> ClResult<Option<Payment>>;

	/// Records what the gateway calls the payment, and where it wants the browser sent, once
	/// `start` has answered.
	///
	/// `false` when the row already carries a `provider_ref`: first write wins, so two
	/// concurrent starts cannot leave the row pointing at the one that was abandoned.
	async fn set_started(
		&self,
		id: i64,
		provider_ref: &str,
		redirect_url: Option<&str>,
	) -> ClResult<bool>;

	/// A status transition guarded by the current status. `false` when the row is not in one
	/// of `from`; an empty `from` matches nothing and answers `false`, and the adapter must
	/// not render it as an unguarded `UPDATE`.
	async fn advance_status(
		&self,
		id: i64,
		to: PaymentState,
		from: &[PaymentState],
	) -> ClResult<bool>;

	/// **One atomic unit.** Moves the payment to `Settlement::to`, stamps `received_at`,
	/// writes the `payment_allocations` row, and refreshes `invoices.paid_amount`, `paid_at`
	/// and `status` from `SUM(payment_allocations.amount)` — all in one transaction.
	///
	/// **`status` is part of the contract, not a cache**: the invoice reads `PAID` once the
	/// sum covers `gross` and `ISSUED` again when a reversal drops it below, so this is the
	/// only path by which a gateway-settled invoice becomes paid. A second adapter that
	/// refreshes only the amounts leaves every settled invoice reading `ISSUED` forever.
	///
	/// `false` means nothing was written at all, and the caller treats it as a conflict, never
	/// as success: either the payment had already left `Settlement::from`, in which case the
	/// allocation would have settled the same money twice, or the invoice is not `ISSUED`/
	/// `PAID`, in which case the money would have landed nowhere.
	///
	/// Partial payment, overpayment and one payment settling two invoices are all this method
	/// called again with a different `amount` or `invoice_id`; none is a special case.
	async fn settle(&self, s: &Settlement) -> ClResult<bool>;

	/// Newest first, filtered by [`PaymentFilter`].
	async fn list_payments(
		&self,
		org_id: i64,
		filter: &PaymentFilter<'_>,
	) -> ClResult<Vec<Payment>>;

	async fn allocations(&self, payment_id: i64) -> ClResult<Vec<PaymentAllocation>>;

	/// Every allocation of every payment named, so a page of payments costs one read rather
	/// than one per row. Ordered by `payment_id`, then `invoice_id`.
	async fn allocations_for(&self, payment_ids: &[i64]) -> ClResult<Vec<PaymentAllocation>>;

	/// Every payment opened against one invoice, newest first. Found through the zero-amount
	/// link row [`Self::create_payment`] writes, so a payment that has not settled yet — the
	/// one a customer coming back to the page needs to resume — is in the answer.
	async fn payments_by_invoice(&self, org_id: i64, invoice_id: i64) -> ClResult<Vec<Payment>>;

	/// `orgs.id` for a public `uid`. Manual entry names its org on the wire
	/// (`POST /api/admin/payments`), because an operator is not confined to `ctx.org_id`.
	async fn org_id_by_uid(&self, uid: &OrgId) -> ClResult<Option<i64>>;

	/// **One atomic unit.** Adds to `payments.refunded_amount`, moves the status, and — when
	/// the payment settled an invoice — writes the negative `payment_allocations` row and
	/// refreshes that invoice's `paid_amount`/`paid_at` cache from the new sum.
	///
	/// `refunded_amount` moves by `RefundRecord::amount`, the allocation by
	/// `-RefundRecord::reverse`: the two are different figures whenever the payment held
	/// money it never allocated. `invoice_id` is `None` whenever `reverse` is zero.
	///
	/// `false` is a conflict, exactly as in [`Self::settle`]: the payment had already left
	/// `RefundRecord::from`, the refund would push `refunded_amount` past `amount`, or
	/// `refunded_amount` moved under the caller and is no longer
	/// `RefundRecord::expect_refunded`.
	async fn record_refund(&self, r: &RefundRecord) -> ClResult<bool>;

	/// Issued invoices past their `due_date` and not yet fully paid, oldest first. `org_id`
	/// `None` is every org, which is what the dunning sweep asks for; the aging list passes
	/// one. Today's date is the store's, so the cutoff and `days_overdue` share one clock.
	/// `after_invoice_id` is exclusive and pages the sweep: the query is not filtered by
	/// "already reminded" — that record is the job's `dedup_key` — so a flat `LIMIT` meant the
	/// same oldest page came back every day and nothing past it was ever dunned.
	async fn overdue_invoices(
		&self,
		org_id: Option<i64>,
		after_invoice_id: Option<i64>,
		limit: i64,
	) -> ClResult<Vec<OverdueInvoice>>;

	/// Money that arrived and settles nothing: `SUCCEEDED` and `PARTIALLY_SUCCEEDED` payments
	/// whose allocations sum to less than `amount - refunded_amount`, received before
	/// `received_before`. The count and the oldest arrival, which is what `A-PAY-UNALLOCATED`
	/// reports.
	///
	/// The partial one belongs here by design: `allocate::apply_state` allocates nothing for it
	/// — `fetch_state` answers with a status and no amount — so it is money waiting on an
	/// operator and nothing else would ever say so.
	///
	/// The clock is `COALESCE(received_at, updated_at)`: only `settle` stamps `received_at`, and
	/// a payment that settled nothing never reached one.
	async fn unallocated_payments(
		&self,
		received_before: Timestamp,
	) -> ClResult<(i64, Option<Timestamp>)>;

	/// Refunds the gateway made that no `payments` row records: the count and the oldest, which
	/// is what `A-PAY-REFUND-UNRECORDED` reports.
	///
	/// Backed by the `PAYMENT_REFUND_UNRECORDED` audit rows `crate::refund` writes when
	/// `record_refund` answers `false` after the payout. It is a read of `audit_logs` rather
	/// than of the money tables because the honest signal is not derivable from them: a refund
	/// that comes out of a payment's *unallocated* part legitimately moves `refunded_amount`
	/// with no reversal row, so a sum comparison flags every overpayment refund.
	///
	/// **Cleared** by a later `PAYMENT_REFUND` row for the same payment: the gateway refund is
	/// keyed on `{payment_uid}:{refunded_amount}`, so retrying the route after the cause is
	/// fixed is the same refund at the gateway and its success writes that row. A discrepancy
	/// reconciled by hand in the database, with no retry, holds the alert — the route retry is
	/// the supported recovery.
	async fn refund_discrepancies(&self) -> ClResult<(i64, Option<Timestamp>)>;

	/// Gateway-backed payments still live, or locally abandoned, untouched since
	/// `updated_before` and born after `created_after`, oldest first — what the sweep re-asks
	/// the gateway about when no callback ever arrived. `CANCELED` is in the list because
	/// `allocate::abandon` writes it without telling the gateway anything.
	///
	/// `canceled_before` is `updated_before` for `CANCELED` rows alone, and is meant to be much
	/// older: `abandon` is a local give-up rather than a gateway fact, so the row still has to
	/// be re-asked, but nobody is waiting on it. With one threshold a backlog of abandoned
	/// payments never moves its `updated_at` and took the whole per-tick ceiling from the front
	/// of the `id` order, so a newly live payment was never re-asked at all.
	///
	/// `after_id` is exclusive and pages the sweep: `apply_state` writes nothing when the
	/// fetched state equals the stored one, so `updated_at` never moves and a flat `LIMIT`
	/// re-read the same oldest page every tick.
	async fn live_payments(
		&self,
		updated_before: Timestamp,
		canceled_before: Timestamp,
		created_after: Timestamp,
		after_id: Option<i64>,
		limit: i64,
	) -> ClResult<Vec<Payment>>;
}

/// The registered [`BillingStore`], or an internal error naming what the application forgot.
pub fn store(app: &saas_core::App) -> ClResult<Arc<dyn BillingStore>> {
	app.extensions
		.get::<Arc<dyn BillingStore>>()
		.cloned()
		.ok_or_else(|| Error::internal("saas-billing: no BillingStore was registered on the app"))
}

// vim: ts=4
