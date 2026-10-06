// SPDX-License-Identifier: MPL-2.0
//! Generic, org-scoped JSON object storage: one trait, no SQL and no pool, the way
//! [`crate::store::CoreStore`] is.
//!
//! Its two consumers are the entity-extension case — a `billing_parties`, `services` or
//! `invoices` row carries extra fields under an object type keyed by that entity's own uid,
//! `invoice.ext` / `inv_<ULID>` — and script-declared object types. It lives in `mintworks-core`
//! rather than in a scripting crate because a Rust consumer wants it too, and putting it
//! elsewhere would force the store adapter to depend on that crate.
//!
//! **Scoping is strict equality.** Every read takes exactly one `org_id` and matches
//! `WHERE org_id = ?`. Nothing descends a subtree — inheritance is a role concept, not a
//! data-visibility one.
//!
//! **Indexed paths are caller-supplied, not adapter-held.** [`ObjectStore::object_put`] is
//! told which paths to index and [`ObjectStore::object_index_reconcile`] is told the full
//! declaration set, so an adapter keeps no registry and a write is correct with no startup
//! ordering to observe.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ClResult;
use crate::types::Timestamp;

/// One stored object, as read back.
///
/// `uid` is the public identifier — the prefixed ULID of the entity for an extension object
/// (`inv_…`, `prt_…`, `svc_…`), or the script's own key. `id` is the internal row id and
/// exists only to be handed back as the next `before_id`; it is never a wire identifier.
#[derive(Clone, Debug)]
pub struct Object {
	pub id: i64,
	pub uid: String,
	/// The object type, e.g. `invoice.ext`. Named `type` in the stored row and on the wire;
	/// `type` is a Rust keyword.
	pub type_name: String,
	pub body: Value,
	pub created_at: Timestamp,
	pub updated_at: Timestamp,
}

/// One declared object type and the JSON paths its queries need indexed.
///
/// `paths` are SQLite-compatible JSON paths — `$.projectUid`, `$.meta.tag` — and are the
/// only thing a declaration changes: the declaration is not a schema change, so it needs no
/// migration and no version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectType {
	pub type_name: String,
	pub paths: Vec<String>,
}

/// Who stamps `created_at` / `updated_at`: the adapter, from its own clock, the way
/// [`crate::audit::log`] stamps `AuditEntry::at`. Neither is a parameter, so a caller cannot
/// write a timestamp that disagrees with the row's own ordering.
///
/// `created_at` survives an overwrite; `updated_at` moves.
///
/// **An extension object stays writable after its entity is immutable.** `ISSUED` immutability
/// guards the invoice document — amounts, lines, buyer snapshot — which the
/// `WHERE id = ? AND status = 'DRAFT'` updates protect and NAV cross-validates. An `invoice.ext`
/// body is not part of it — no implementation may read or check an entity's status.
///
/// **Authorization and audit are the caller's, and are not optional.** No method here takes a
/// `&Ctx`, so nothing in the store can derive a scope or write a row; the layering rule puts
/// both in the service handle. The caller derives `org_id` from `ctx.actor` and never from a
/// request field — a body is reachable by `(org_id, type, uid)` alone, so an org taken from the
/// request is a cross-tenant read — and writes a [`crate::audit`] row for every `object_put` and
/// `object_delete`, because this store writes none and an `invoice.ext` mutation on an `ISSUED`
/// invoice would otherwise leave no trace at all.
#[async_trait]
pub trait ObjectStore: Send + Sync + 'static {
	/// Insert or overwrite the object at `(org_id, type_name, uid)`, and rewrite its index
	/// rows from `indexed` in the same transaction as the body.
	///
	/// `UNIQUE (org_id, type, uid)` is what makes this an upsert rather than a duplicate:
	/// the same `uid` in another org, or under another type, is a different object.
	///
	/// `indexed` is the declared path list for this type — the caller's, because the adapter
	/// holds no declarations. A path in `indexed` the body lacks indexes as null, not skipped; a
	/// path repeated in `indexed` is indexed once, not an error; a path **removed** since an
	/// earlier write is dropped only by [`ObjectStore::object_index_reconcile`].
	async fn object_put(
		&self,
		org_id: i64,
		type_name: &str,
		uid: &str,
		body: &Value,
		indexed: &[String],
	) -> ClResult<Object>;

	/// The object at `(org_id, type_name, uid)`, or `None` — including when the object exists
	/// under that uid in a **different org or under a different type**, which is a miss and
	/// never an error.
	async fn object_get(&self, org_id: i64, type_name: &str, uid: &str)
	-> ClResult<Option<Object>>;

	/// Delete the object and its index rows. `Ok(false)` when no row matched, so a repeated
	/// delete is a no-op rather than an error.
	async fn object_delete(&self, org_id: i64, type_name: &str, uid: &str) -> ClResult<bool>;

	/// A page of one type, newest first — `id DESC`, the order [`crate::store::CoreStore`]'s
	/// job and audit reads use. Pass the last `Object::id` seen as `before_id`; `None` starts
	/// at the newest.
	///
	/// `limit` is the page size the caller has already clamped. The store does not encode a
	/// cursor: a caller that wants an opaque wire cursor encodes the last `id` itself, so no
	/// encoding version has to be shared between two adapters.
	async fn object_list(
		&self,
		org_id: i64,
		type_name: &str,
		before_id: Option<i64>,
		limit: i64,
	) -> ClResult<Vec<Object>>;

	/// [`ObjectStore::object_list`] narrowed to objects whose **indexed** path `path` holds
	/// exactly `value`, newest first and paged the same way.
	///
	/// `value` is the scalar's JSON text as it was extracted, so a caller matching a number
	/// or a string spells it the way the body holds it. An object whose body lacks `path`
	/// indexed as null and does not match any `value` here; to find those, read the page and
	/// filter.
	///
	/// A `path` that was never declared has no index rows, so this returns nothing rather
	/// than failing — the declaration set is the caller's, and an adapter cannot distinguish
	/// "undeclared" from "declared, and nothing matched".
	async fn object_query(
		&self,
		org_id: i64,
		type_name: &str,
		path: &str,
		value: &str,
		before_id: Option<i64>,
		limit: i64,
	) -> ClResult<Vec<Object>>;

	/// Reconcile the index against the declared set: add rows for paths now declared, drop
	/// rows for paths no longer declared.
	///
	/// This is where a declaration change lands, and it is org-agnostic: a path is declared for a
	/// *type*, not per org. Run it at startup, inside one transaction, before anything queries the
	/// affected paths — a half-applied reconcile leaves queries answering from a partial index.
	///
	/// `declared` holds **at most one entry per type**. A repeated `type_name` is an error, not
	/// a merge of the two path lists: merging is how a consumer silently shadows a framework
	/// extension type (`invoice.ext`, `party.ext`, `service.ext`), which nothing reserves.
	///
	/// Leaves the objects themselves alone. A path dropped from the declaration loses its
	/// index rows and its values; the bodies keep the fields.
	async fn object_index_reconcile(&self, declared: &[ObjectType]) -> ClResult<()>;
}

// vim: ts=4
