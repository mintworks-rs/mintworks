// SPDX-License-Identifier: MPL-2.0
//! A process-local read-through cache a concurrent write cannot poison.
//!
//! [`crate::settings::Settings`] and [`crate::secrets::SecretStore`] are both read-then-insert
//! over the reader pool while their `set` writes and then invalidates. Interleaved, the stale
//! read wins: A misses, A reads the DB and sees the old value (or nothing), B writes the row
//! and clears the entry, A then inserts what it saw. The entry is pinned until restart — for
//! `auth.jwt_key` that is a 401 on every request, for `nav.sign_key` every filing failing.
//!
//! Re-checking under the write lock does not help, because `set` has already removed the key
//! and the re-check misses too. A generation counter does: it is bumped in the same critical
//! section as the invalidation, so a `get` can tell that the world moved under it and decline
//! to cache what it read.
//!
//! On top of that, every entry expires. `invalidate` reaches only the calling process's map,
//! and multi-process is anticipated — `SqliteStore::write_tx` reasons about "a second process
//! on the same file" — so without a TTL an `auth.jwt_key` rotation left every *other* process
//! signing with the old key until restart, with no upper bound.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::RwLock;

/// How long a cached entry stands before it is re-read.
///
/// Covers the write `invalidate` can never hear about: another **process**'s. 30 s bounds it at
/// one reader-pool round trip per key per half-minute.
///
/// A convergence window, not cross-process invalidation: a rotation is visible everywhere
/// within `TTL` rather than at once. Instant would need a broadcast (`NOTIFY`, a bus message)
/// calling `invalidate` on every process, with this as the backstop.
const TTL: Duration = Duration::from_secs(30);

/// Ceiling on live entries, because a caller may key on something the caller's *user* chooses.
const MAX_ENTRIES: usize = 4096;

pub struct GenCache<V> {
	inner: RwLock<Inner<V>>,
}

struct Inner<V> {
	generation: u64,
	/// `Instant`, never `SystemTime`: monotonic, so a clock step cannot extend a stale entry.
	entries: HashMap<String, (V, Instant)>,
}

/// What [`GenCache::lookup`] returns on a miss: the generation to hand back to
/// [`GenCache::store`], so the insert can be dropped if a write landed in between.
#[derive(Clone, Copy)]
pub struct Miss(u64);

impl<V: Clone> Default for GenCache<V> {
	fn default() -> Self {
		Self::new()
	}
}

impl<V: Clone> GenCache<V> {
	pub fn new() -> Self {
		Self { inner: RwLock::new(Inner { generation: 0, entries: HashMap::new() }) }
	}

	/// An entry older than [`TTL`] reads as a miss. The stale row is left where it is rather
	/// than removed — the read lock is the common path, and the `store` that follows this miss
	/// overwrites it anyway.
	pub fn lookup(&self, key: &str) -> Result<V, Miss> {
		let inner = self.inner.read();
		match inner.entries.get(key) {
			Some((v, at)) if at.elapsed() < TTL => Ok(v.clone()),
			_ => Err(Miss(inner.generation)),
		}
	}

	pub fn store(&self, key: &str, miss: Miss, value: V) {
		let mut inner = self.inner.write();
		if inner.generation != miss.0 {
			return;
		}
		// An expired entry is never removed on its own, and `mintworks_auth::token` keys on
		// `accounts.locale`, which `is_locale` admits millions of spellings of. Sweep the
		// expired ones at the ceiling, and drop the lot if they were all live.
		if inner.entries.len() >= MAX_ENTRIES {
			inner.entries.retain(|_, (_, at)| at.elapsed() < TTL);
			if inner.entries.len() >= MAX_ENTRIES {
				inner.entries.clear();
			}
		}
		inner.entries.insert(key.to_owned(), (value, Instant::now()));
	}

	/// Drop every entry, for an invalidation that cannot name the keys it affects —
	/// `current_legal_doc` falls back across locales, so one publish moves every locale's answer.
	pub fn clear(&self) {
		let mut inner = self.inner.write();
		inner.entries.clear();
		inner.generation += 1;
	}

	/// Drop the entry and move the generation on, so any `get` still in flight discards what
	/// it read rather than pinning it.
	pub fn invalidate(&self, key: &str) {
		let mut inner = self.inner.write();
		inner.entries.remove(key);
		inner.generation += 1;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The interleaving: a `get` misses, a `set` lands, the `get` then tries to cache what it
	/// read before the write. Caching it there poisoned the entry until process restart.
	#[test]
	fn a_write_that_lands_mid_read_discards_the_stale_insert() {
		let cache: GenCache<Option<&str>> = GenCache::new();

		// Task A misses and reads the DB, which has nothing yet.
		let Err(miss) = cache.lookup("k") else { panic!("unexpected hit") };
		// Task B writes the row and invalidates.
		cache.invalidate("k");
		// Task A now tries to cache the `None` it saw.
		cache.store("k", miss, None);

		assert!(cache.lookup("k").is_err(), "the stale read must not be cached");

		// A read with no write racing it caches normally.
		let Err(miss) = cache.lookup("k") else { panic!("unexpected hit") };
		cache.store("k", miss, Some("v"));
		assert_eq!(cache.lookup("k").ok(), Some(Some("v")));
	}

	/// The other process's write, which `invalidate` can never hear about.
	#[test]
	fn an_entry_older_than_the_ttl_reads_as_a_miss() {
		let cache: GenCache<&str> = GenCache::new();
		let Err(miss) = cache.lookup("k") else { panic!("unexpected hit") };
		cache.store("k", miss, "v");
		assert_eq!(cache.lookup("k").ok(), Some("v"));

		// Age the entry rather than sleeping 30 seconds for it.
		let mut inner = cache.inner.write();
		let at = &mut inner.entries.get_mut("k").unwrap().1;
		*at = at.checked_sub(TTL).expect("the test machine booted less than a TTL ago");
		drop(inner);

		assert!(cache.lookup("k").is_err(), "a stale entry must be re-read, not served");
		// And re-reading it puts a fresh one back.
		let Err(miss) = cache.lookup("k") else { panic!("unexpected hit") };
		cache.store("k", miss, "w");
		assert_eq!(cache.lookup("k").ok(), Some("w"));
	}
}

// vim: ts=4
