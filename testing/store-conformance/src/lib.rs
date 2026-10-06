//! The store-adapter conformance suite: the tests a store adapter must pass, written once and
//! generic over [`Harness`] so every backend runs the same assertions.
//!
//! Each module is a set of `pub async fn <test>::<H: Harness>()` whose bounds name only the store
//! traits that module uses, so a backend opts in module by module. Its `<module>_tests!` macro
//! expands to one `#[tokio::test]` per test; an adapter invokes it from its own `tests/` with its
//! harness type: `store_conformance::tx_tests!(SqliteHarness);`. The caller supplies `tokio`.
//!
//! Raw SQL here uses `?` placeholders and portable SQL; a backend whose driver wants `$n`
//! rewrites them in its harness. Anything only one backend can say stays in that adapter's tests.
//!
//! The app DB has its own harness, [`AppDbHarness`], and its own modules (`appdb`, and under
//! `ai` `memory` and `thread`), run by every `AppDb` adapter the same way.

#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// The suite is driven from `#[tokio::test]` on one task; nothing here needs `Send` futures.
#![allow(async_fn_in_trait)]

use saas_core::ClResult;
use serde_json::Value;

#[cfg(feature = "ai")]
pub mod agent;
pub mod appdb;
pub mod auth;
pub mod billing;
pub mod entitle;
pub mod invoice;
pub mod job_claim;
#[cfg(feature = "ai")]
pub mod llm;
#[cfg(feature = "ai")]
pub mod memory;
pub mod objects;
pub mod pdf;
pub mod plans;
pub mod refs;
#[cfg(feature = "ai")]
pub mod search;
#[cfg(feature = "ai")]
pub mod thread;
pub mod tx;

/// One backend under test: a freshly migrated database and the handles the suite needs.
pub trait Harness: Sized + 'static {
	/// The store handle; implements whichever store traits the backend supports.
	type Store: Clone + Send + Sync + 'static;
	/// A write transaction, as returned by [`Harness::begin`] / [`Harness::write_tx`].
	type Tx: Send + 'static;

	/// A new, migrated (framework module) database private to the test `name`. `None` means the
	/// backend is unavailable here (no server configured) and the test returns early.
	async fn fresh(name: &str) -> Option<Self>;
	/// The store over the fresh database.
	fn store(&self) -> &Self::Store;
	/// A second, independent store over the same database, for the contention tests.
	async fn reopen(&self) -> Self::Store;

	/// Runs a statement outside any transaction; returns the rows affected. Args bind in order:
	/// a JSON integer as an integer, a string as text, `null` as NULL.
	async fn exec(&self, sql: &str, args: &[Value]) -> u64 {
		self.try_exec(sql, args).await.unwrap()
	}
	/// [`Harness::exec`] for a statement the test expects the database to refuse; the error is
	/// the driver's message.
	async fn try_exec(&self, sql: &str, args: &[Value]) -> Result<u64, String>;
	/// Runs a query outside any transaction; each row is its columns in order, integers as JSON
	/// numbers, text as strings, NULL as `null`.
	async fn rows(&self, sql: &str, args: &[Value]) -> Vec<Vec<Value>>;

	/// A write transaction plus a clone of `store` bound to it; nests when `store` is bound.
	async fn begin(store: &Self::Store) -> ClResult<(Self::Tx, Self::Store)>;
	/// A write transaction on `store` (a savepoint when `store` is bound).
	async fn write_tx(store: &Self::Store) -> ClResult<Self::Tx>;
	async fn commit(tx: Self::Tx) -> ClResult<()>;
	async fn rollback(tx: Self::Tx) -> ClResult<()>;
	/// Runs a statement on the connection `tx` holds, inside it.
	async fn tx_exec(tx: &Self::Tx, sql: &str, args: &[Value]) -> u64;
	/// Runs `fut` with every pooled handle on this task joined to `tx`.
	async fn scope_writes<T>(tx: &Self::Tx, fut: impl Future<Output = T>) -> T;

	/// The single integer the query answers.
	async fn scalar_i64(&self, sql: &str, args: &[Value]) -> i64 {
		self.opt_i64(sql, args).await.expect("the query answered NULL")
	}
	/// The single text value the query answers.
	async fn scalar_text(&self, sql: &str, args: &[Value]) -> String {
		self.opt_text(sql, args).await.expect("the query answered NULL")
	}
	/// The first column of the first row, `None` for no row or NULL.
	async fn opt_i64(&self, sql: &str, args: &[Value]) -> Option<i64> {
		first(self.rows(sql, args).await).map(|v| v.as_i64().expect("not an integer"))
	}
	/// The first column of the first row, `None` for no row or NULL.
	async fn opt_text(&self, sql: &str, args: &[Value]) -> Option<String> {
		first(self.rows(sql, args).await).map(|v| v.as_str().expect("not text").to_owned())
	}
}

/// A migration module the app-DB suite asks for; each harness maps it onto its adapter's own
/// `Module`, since a module's `apply` takes the adapter's connection type.
#[derive(Clone, Copy, Debug)]
pub enum TestModule {
	/// `notes`: creates `notes (id <auto-assigned integer key>, body TEXT)`; not idempotent.
	Notes { version: i64 },
	/// `seed` version 1: inserts one `notes` row with body `'seed'`, so it fails unless `notes`
	/// already exists.
	Seed,
	/// The adapter's `MEMORY` content module.
	#[cfg(feature = "ai")]
	Memory,
	/// The adapter's `AGENT` content module.
	#[cfg(feature = "ai")]
	Agent,
}

/// One app-DB backend under test: a fresh, empty database (no module applied).
pub trait AppDbHarness: Sized + 'static {
	/// The app-DB handle; implements `AppDb` and, under `ai`, `MemoryStore` + `ThreadStore`.
	type Db: Send + Sync + 'static;

	/// A new, empty database private to the test `name`. `None` means the backend is
	/// unavailable here and the test returns early.
	async fn fresh(name: &str) -> Option<Self>;
	/// The handle over the fresh database.
	fn db(&self) -> &Self::Db;
	/// A second handle over the same database, as a reboot would open it.
	fn open(&self) -> Self::Db;
	/// `sql` in the adapter's dialect: the `?` placeholders rewritten where the driver wants `$n`.
	fn sql(sql: &str) -> String;
	/// The adapter's module runner over `modules`, in list order.
	async fn migrate(db: &Self::Db, modules: &[TestModule]) -> ClResult<()>;
	/// The names of the `ix:` indexes `reconcile` created, sorted.
	async fn index_names(db: &Self::Db) -> Vec<String>;
}

fn first(rows: Vec<Vec<Value>>) -> Option<Value> {
	rows.into_iter().next()?.into_iter().next().filter(|v| !v.is_null())
}

/// Waits for work a `Drop` handed to the runtime. Nothing signals it, so the choice is a poll or
/// a fixed sleep long enough to be slower than the test.
pub async fn eventually(mut done: impl AsyncFnMut() -> bool) {
	for _ in 0..200 {
		if done().await {
			return;
		}
		tokio::time::sleep(std::time::Duration::from_millis(10)).await;
	}
	panic!("the spawned work never finished");
}

/// Opens the harness for test `$name`, or returns from the test when the backend is unavailable.
#[macro_export]
#[doc(hidden)]
macro_rules! fresh {
	($h:ty, $name:expr) => {
		match <$h as $crate::Harness>::fresh($name).await {
			Some(h) => h,
			None => return,
		}
	};
}

/// [`fresh!`] for an [`AppDbHarness`].
#[macro_export]
#[doc(hidden)]
macro_rules! fresh_db {
	($h:ty, $name:expr) => {
		match <$h as $crate::AppDbHarness>::fresh($name).await {
			Some(h) => h,
			None => return,
		}
	};
}

/// Expands to one `#[tokio::test]` per listed test fn of `$module`, on the default runtime or on
/// a 4-worker multi-thread one (the contention tests).
#[macro_export]
#[doc(hidden)]
macro_rules! __tests {
	($h:ty, $module:ident, [$($name:ident),* $(,)?], multi_thread: [$($mt:ident),* $(,)?]) => {
		$(
			#[tokio::test]
			async fn $name() {
				$crate::$module::$name::<$h>().await;
			}
		)*
		$(
			#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
			async fn $mt() {
				$crate::$module::$mt::<$h>().await;
			}
		)*
	};
}

/// The [`tx`] module's tests; the store needs `CoreStore + ObjectStore`.
#[macro_export]
macro_rules! tx_tests {
	($h:ty) => {
		$crate::__tests!($h, tx, [
			a_write_through_a_bound_handle_joins_the_transaction,
			two_writes_joined_on_one_task_do_not_share_a_transaction,
			an_audit_row_raised_on_a_bound_handle_survives_a_rollback,
			an_audit_row_written_on_a_bound_handle_rolls_back_with_it,
			a_write_through_a_bound_handle_after_commit_is_an_error,
			a_write_through_a_bound_handle_after_drop_is_an_error,
			a_second_nested_write_tx_is_rejected_not_stacked,
			bound_handle_reads_its_own_writes,
			commit_returns_the_writer_connection,
			nested_bound_handle_errors_after_release,
			dropped_nested_tx_does_not_poison_a_later_statement,
			nested_scopes_reopen_in_sequence,
			an_ancestor_commit_ends_the_scopes_inside_it,
			commit_flushes_buffered_audit,
			dropped_tx_flushes_buffered_audit,
			a_pooled_handle_inside_scope_writes_joins_the_transaction,
			a_bound_handle_wins_over_the_ambient_scope,
			a_spawned_task_does_not_inherit_the_ambient_scope,
			the_ambient_scope_ends_with_the_future,
		], multi_thread: []);
	};
}

/// The [`job_claim`] module's tests; the store needs `CoreStore`.
#[macro_export]
macro_rules! job_claim_tests {
	($h:ty) => {
		$crate::__tests!($h, job_claim, [
			a_reclaim_leaves_a_freshly_claimed_row_to_its_owner,
			job_wake_moves_only_pending_rows_forward,
			job_defer_is_running_guarded_and_clears_the_failure,
			job_redrive_done_restores_the_payload_job_complete_blanked,
			job_statuses_by_keys_answers_only_for_the_keys_asked_for,
			job_statuses_by_keys_spans_more_keys_than_sqlite_binds_in_one_statement,
		], multi_thread: [
			a_job_is_claimed_by_exactly_one_worker,
			two_stores_seeding_the_same_periodic_kind_insert_one_row,
		]);
	};
}

/// The [`auth`] module's tests; the store needs `AuthStore + CoreStore`.
#[macro_export]
macro_rules! auth_tests {
	($h:ty) => {
		$crate::__tests!($h, auth, [
			anonymize_account_refuses_an_account_that_still_owns_an_organisation,
			erasure_revokes_the_subjects_organisation_keys,
			removing_a_membership_revokes_the_member_at_once,
			the_owner_membership_survives_a_concurrent_remove,
			a_recovery_code_set_can_only_be_swapped_once,
			an_enrolment_cannot_wipe_a_confirmed_credential,
			a_totp_code_cannot_be_spent_twice,
			activation_sets_an_invited_accounts_first_password,
			a_new_accounts_own_org_is_already_accepted,
			confirming_a_factor_arms_it_and_stores_its_recovery_codes_together,
			erasure_reaches_every_table_in_one_transaction,
			a_failed_login_counts_and_an_unknown_account_costs_the_same_write,
			one_org_cannot_revoke_another_orgs_api_key,
			api_key_liveness_carries_membership_account_and_org,
			an_erased_account_cannot_be_brought_back,
			suspending_an_account_bumps_its_token_epoch_atomically,
			latest_consent_and_list_consents_agree_across_a_clock_step_back,
			a_role_on_an_ancestor_resolves_on_every_descendant,
			the_ancestor_walk_terminates_on_a_cycle,
			the_root_org_is_never_deletable,
			the_root_org_cannot_be_suspended,
			the_root_is_found_by_kind_not_by_being_parentless,
			a_key_on_a_child_org_is_live_through_an_ancestor_membership,
			a_webauthn_credential_round_trips_and_a_duplicate_id_conflicts,
			put_webauthn_credential_refuses_past_the_cap,
			an_expired_api_key_does_not_count_against_the_live_cap,
		], multi_thread: []);
	};
}

/// The [`auth`] tests that also reach objects and refs; the store needs
/// `AuthStore + CoreStore + ObjectStore + RefStore`.
#[macro_export]
macro_rules! auth_ext_tests {
	($h:ty) => {
		$crate::__tests!($h, auth, [
			erasure_blanks_object_body_to_valid_json,
			a_non_cascading_reference_keeps_an_org_undeletable,
			an_org_that_sent_an_invite_is_deletable,
		], multi_thread: []);
	};
}

/// The [`invoice`] module's tests; the store needs `InvoiceStore + NavStore`.
#[macro_export]
macro_rules! invoice_tests {
	($h:ty) => {
		$crate::__tests!($h, invoice, [
			rolled_back_issue_consumes_no_number,
			issued_invoice_cannot_be_mutated,
			no_trait_path_leads_out_of_a_terminal_status,
			a_pending_invoice_is_frozen_but_still_issues,
			the_sweep_spares_a_draft_that_is_still_being_edited,
			the_sweep_collects_a_dead_lock_and_spares_a_live_one,
			storno_negates_and_happens_once,
			a_conflict_without_a_request_id_does_not_name_one,
			two_orgs_can_hold_the_same_request_id,
			a_custom_series_format_survives_the_year_boundary,
			the_pdf_sweep_returns_the_least_recently_attempted_first,
			a_currency_refuses_a_markup_or_a_step_that_would_misprice,
			a_storno_always_has_a_numbered_original_to_name,
			repeated_draft_saves_rewrite_one_row_and_publish_nothing,
			publishing_archives_the_live_version_and_promotes_the_draft,
			sync_seller_version_never_publishes_a_concurrent_draft,
			publishing_with_no_draft_changes_nothing,
			discarding_a_draft_leaves_the_live_version_untouched,
			two_racing_publishes_cannot_leave_two_live_versions,
			an_issued_invoice_resolves_the_version_it_froze,
			request_archived_tracks_the_archived_request,
			releasing_a_pristine_member_takes_its_archive_with_it,
			archiving_for_a_deleted_submission_is_a_no_op,
			a_batch_read_does_not_carry_the_archive,
			set_status_refuses_any_pair_but_the_gateway_lock,
			two_sellers_number_independently,
			create_seller_only_mints_on_an_active_shared_org,
			create_seller_never_overwrites_an_existing_row,
			put_seller_cannot_move_a_seller_to_another_org,
			put_seller_refuses_a_changed_uid_on_the_same_id,
			put_seller_refuses_a_reused_uid,
			summary_groups_by_status_and_currency,
			summary_months_use_fulfilment_date,
			summary_excludes_drafts_from_months,
			summary_storno_nets_out,
			summary_overdue_ignores_paid_and_future,
			summary_paid_this_month_is_by_payment_date,
			summary_empty_org_is_empty,
			list_filters_by_status,
			list_filters_by_number_and_buyer_name,
			list_finds_draft_by_party_name,
			list_q_escapes_like_wildcards,
			list_filter_pages_with_cursor,
			seller_has_issued_ignores_drafts,
			closing_is_refused_while_a_payment_is_pending,
			payment_terms_round_trip_and_put_seller_keeps_them,
			put_seller_does_not_reopen,
		], multi_thread: [
			concurrent_issue_is_unique_and_gapless,
		]);
	};
}

/// The [`billing`] module's tests; the store needs `BillingStore + InvoiceStore`.
#[macro_export]
macro_rules! billing_tests {
	($h:ty) => {
		$crate::__tests!($h, billing, [
			create_payment_writes_the_zero_link_row,
			a_spent_request_id_conflicts,
			the_allocation_ceiling_holds_under_two_writers,
			overdue_invoices_page_past_the_first_batch,
			advance_status_is_guarded_by_the_current_status,
			settle_against_a_draft_rolls_everything_back,
			a_second_allocation_sums_onto_the_same_row,
			set_started_records_the_gateway_reference_and_its_redirect,
			a_draft_with_an_abandoned_payment_is_still_deletable,
			payments_by_invoice_finds_the_unsettled_one,
			org_id_by_uid_resolves_the_public_id,
			list_payments_is_org_scoped_and_newest_first,
			live_payments_finds_stale_gateway_backed_rows_at_any_age,
			a_canceled_payment_is_never_selected,
			record_refund_is_guarded_by_the_current_status,
			a_refund_past_the_payment_is_refused,
			a_refund_reverses_the_allocation_and_the_invoice_cache,
			a_refund_against_a_stornoed_invoice_still_records,
			overdue_invoices_is_the_aging_list,
			payment_by_uid_none_crosses_orgs,
			an_empty_from_moves_nothing,
			a_partially_refunded_payment_is_not_unallocated,
			a_retried_refund_clears_its_discrepancy,
			allocations_for_reads_a_page_at_once,
			sweep_drafts_leaves_a_draft_with_a_live_payment,
			sweep_drafts_leaves_an_unissued_invoice_that_was_paid,
			a_provider_reference_is_claimed_once,
			a_partial_that_settles_nothing_is_unallocated_money,
		], multi_thread: []);
	};
}

/// The [`objects`] module's tests; the store needs `ObjectStore + InvoiceStore`.
#[macro_export]
macro_rules! objects_tests {
	($h:ty) => {
		$crate::__tests!($h, objects, [
			put_get_list_and_delete_round_trip,
			one_row_per_org_type_and_uid,
			a_read_never_crosses_org,
			query_matches_one_declared_indexed_path,
			overwrite_reindexes_the_declared_paths,
			reconcile_adds_and_drops_a_declared_path,
			a_repeated_indexed_path_is_indexed_once,
			a_type_declared_twice_is_rejected,
			an_unchanged_declaration_skips_the_reconcile,
			deleting_an_org_cascades_its_objects,
			deleting_a_draft_takes_its_ext_blob,
			the_stale_draft_sweep_takes_the_ext_blob_with_it,
			a_party_delete_that_matches_nothing_leaves_the_ext_blob,
			deleting_a_party_takes_its_ext_blob,
		], multi_thread: []);
	};
}

/// The [`pdf`] module's tests; the store needs `DocumentStore + CoreStore` (one test also `InvoiceStore`).
#[macro_export]
macro_rules! pdf_tests {
	($h:ty) => {
		$crate::__tests!($h, pdf, [
			a_document_is_pending_until_rendered_and_confined_to_its_org,
			deleting_the_org_cascades_to_its_documents,
			account_erasure_returns_only_unshared_files,
			a_job_result_is_read_back_by_its_dedup_key,
		], multi_thread: []);
	};
}

/// The [`plans`] module's tests; the store needs `PlanStore + CoreStore` (three also `InvoiceStore`).
#[macro_export]
macro_rules! plans_tests {
	($h:ty) => {
		$crate::__tests!($h, plans, [
			reconcile_upserts_and_deactivates,
			an_undeclared_entitlement_or_service_fails_before_any_write,
			one_live_subscription_per_family,
			a_conflict_leaves_the_outer_transaction_usable,
			a_deleted_sub_was_never_in_the_family,
			sub_save_persists_the_billing_anchor,
			plan_invoice_links_an_invoice_to_its_subscription,
			oldest_unpaid_skips_paid_and_draft_links,
			sweep_drafts_keeps_a_live_subscriptions_renewal,
		], multi_thread: []);
	};
}

/// The [`refs`] module's tests; the store needs `RefStore + CoreStore`.
#[macro_export]
macro_rules! refs_tests {
	($h:ty) => {
		$crate::__tests!($h, refs, [
			insert_and_read_back_case_insensitively,
			redeem_decrements_once_and_repeat_is_idempotent,
			expired_and_revoked_refuse,
			concurrent_redeem_of_a_single_use_succeeds_once,
			concurrent_redeem_by_one_org_succeeds_once,
			a_hold_whose_draft_is_gone_is_reclaimed,
		], multi_thread: []);
	};
}

/// The [`entitle`] module's tests; the store needs `EntitleStore + CoreStore`.
#[macro_export]
macro_rules! entitle_tests {
	($h:ty) => {
		$crate::__tests!($h, entitle, [
			debit_drains_the_soonest_expiring_grant_first,
			a_retried_idem_key_debits_once,
			a_reused_idem_key_with_another_debit_is_refused,
			charge_overdraws_onto_the_last_grant_drained,
			charge_with_no_grant_goes_negative_and_a_later_grant_nets_it,
			an_overdraft_is_forgiven_when_its_grant_expires,
			concurrent_consumes_never_overdraw,
			cut_ends_the_grant_and_keeps_its_usage,
			grant_insert_is_idempotent_on_its_source_ref,
		], multi_thread: []);
	};
}

/// The `agent` module's tests (feature `ai`); the store needs `AgentRunStore`.
#[cfg(feature = "ai")]
#[macro_export]
macro_rules! agent_tests {
	($h:ty) => {
		$crate::__tests!($h, agent, [
			one_live_run_per_thread,
			status_stamps_start_and_finish,
			events_are_sequenced_per_run,
			sweep_interrupts_only_live_runs,
			sweep_takes_only_expired_leases,
		], multi_thread: []);
	};
}

/// The `llm` module's tests (feature `ai`); the store needs `LlmStore`.
#[cfg(feature = "ai")]
#[macro_export]
macro_rules! llm_tests {
	($h:ty) => {
		$crate::__tests!($h, llm, [
			usage_sums_by_subject_and_since,
			budget_is_absent_until_set_and_upserts,
		], multi_thread: []);
	};
}

/// The `search` module's tests (feature `ai`); the store needs `SearchStore`.
#[cfg(feature = "ai")]
#[macro_export]
macro_rules! search_tests {
	($h:ty) => {
		$crate::__tests!($h, search, [
			source_fresh_returns_the_newest_row_inside_the_cutoff,
			search_cache_upserts_per_full_key,
		], multi_thread: []);
	};
}

/// The [`appdb`] module's tests; the harness's `Db` needs `AppDb`.
#[macro_export]
macro_rules! appdb_tests {
	($h:ty) => {
		$crate::__tests!($h, appdb, [
			reconcile_creates_then_adds_a_column,
			a_column_type_change_is_a_hard_error,
			a_committed_block_lands_and_a_failed_one_does_not,
			a_read_inside_a_block_sees_its_own_writes,
			a_duplicate_key_is_a_conflict,
			a_nested_block_is_refused,
			index_names_do_not_collide,
			reserved_words_declare,
			a_cancelled_tx_does_not_poison_the_writer,
			a_query_over_max_rows_is_refused,
			migrate_stamps_the_version_on_a_fresh_file,
			migrate_again_at_the_same_version_is_a_no_op,
			migrate_refuses_a_version_newer_than_the_build,
			migrate_applies_modules_in_list_order,
			a_float_round_trips,
			app_migrations_apply_once,
			an_app_version_above_the_declared_fails,
			a_returning_write_through_query_needs_a_tx,
		], multi_thread: []);
	};
}

/// The `memory` module's tests (feature `ai`); the harness's `Db` needs `MemoryStore`.
#[cfg(feature = "ai")]
#[macro_export]
macro_rules! memory_tests {
	($h:ty) => {
		$crate::__tests!($h, memory, [
			spaces_and_docs_are_unique_per_owner,
			versions_are_immutable_and_append_concatenates,
			search_sees_only_current_bodies_in_scope,
			org_erase_removes_only_that_org,
		], multi_thread: []);
	};
}

/// The `thread` module's tests (feature `ai`); the harness's `Db` needs `ThreadStore`.
#[cfg(feature = "ai")]
#[macro_export]
macro_rules! thread_tests {
	($h:ty) => {
		$crate::__tests!($h, thread, [
			a_title_round_trips,
			threads_are_created_read_and_listed_per_org,
			append_updates_the_thread_and_keeps_order,
			compact_hides_old_messages_but_keeps_them,
			org_erase_removes_only_that_org,
		], multi_thread: []);
	};
}

// vim: ts=4
