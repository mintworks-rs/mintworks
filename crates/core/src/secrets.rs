// SPDX-License-Identifier: MPL-2.0
//! The `secrets` table: AES-256-GCM ciphertext under `HKDF(MASTER_KEY, key)`.
//!
//! Deriving a distinct key per secret name means a row moved to another name fails
//! authentication rather than decrypting, so the name is bound to the ciphertext without
//! a separate AAD.
//!
//! A read resolves the environment **first**, then the encrypted row — the *opposite* of
//! [`crate::settings::Settings`], deliberately. An operator edits a setting through the admin
//! API and must win; a secret in the environment must never be written back to the table, so
//! rotation is a redeploy and a multi-replica deployment shares one `auth.jwt_key` instead of
//! each replica minting its own. The trade-off taken with it: a secret in the environment is
//! readable from `docker inspect` and `/proc/<pid>/environ`, which the table is not.
//!
//! The variable is [`crate::settings::env_name`], the same unprefixed rule a setting uses, and
//! the name is declared through `AppBuilder::secrets` so the two namespaces cannot collide.
//!
//! [`SecretStore::get`] returns raw bytes to framework code only. Nothing here is
//! serializable: the only type that reaches an HTTP layer is [`SecretStatus`], which
//! carries no value.

use std::sync::Arc;

use aes_gcm::{
	Aes256Gcm, Key,
	aead::{Aead, AeadCore, KeyInit, OsRng},
};
use hkdf::Hkdf;
use serde::Serialize;
use sha2::Sha256;

use crate::{
	error::{ClResult, Error, StatusCode},
	gencache::GenCache,
	settings::Registry,
	store::CoreStore,
	types::Timestamp,
};

/// `mintworks-core`'s own secret names, always registered. `auth.jwt_key` is here rather than in
/// `mintworks-auth` because `auth_mw` — this crate — is what verifies the token.
pub static SECRETS: &[&str] = &["auth.jwt_key"];

/// What the admin API is allowed to know about a secret.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretStatus {
	pub set: bool,
	pub updated_at: Option<Timestamp>,
}

/// Reads go through a process-local plaintext cache, invalidated on [`SecretStore::set`],
/// mirroring [`crate::settings::Settings`]. `auth_mw` reads the JWT signing key on every
/// authenticated request, and without the cache that is a DB round trip per request.
///
/// The reader/writer split this needs — a service method holding `write_tx()` that reads an
/// uncached secret would otherwise wait on the connection it is itself holding — is the
/// adapter's obligation behind [`CoreStore`].
pub struct SecretStore {
	store: Arc<dyn CoreStore>,
	master_key: [u8; 32],
	cache: GenCache<Option<Vec<u8>>>,
	/// The one process-wide environment snapshot, shared with [`crate::settings::Settings`]:
	/// `std::env::var` takes a process-wide lock, and `auth_mw` resolves `auth.jwt_key` on
	/// every request.
	registry: Arc<Registry>,
}

impl SecretStore {
	pub fn new(store: Arc<dyn CoreStore>, master_key: [u8; 32], registry: Arc<Registry>) -> Self {
		Self { store, master_key, cache: GenCache::new(), registry }
	}

	/// The secret key names the registered crates declared, for the admin key list.
	#[must_use]
	pub fn declared(&self) -> &[&'static str] {
		self.registry.secrets()
	}

	/// The environment's value for a secret, which **wins over the stored row**: the deployment
	/// is the source of truth and rotation is a redeploy. Blank is absent, so a `.env.example`
	/// copied with empty values cannot shadow a real row with an empty secret.
	fn env_value(&self, key: &str) -> Option<Vec<u8>> {
		self.registry.env(key).map(|v| v.trim().as_bytes().to_vec())
	}

	fn cipher(&self, key: &str) -> ClResult<Aes256Gcm> {
		let mut derived = [0u8; 32];
		Hkdf::<Sha256>::new(None, &self.master_key)
			.expand(key.as_bytes(), &mut derived)
			.map_err(|_| Error::internal("secret key derivation failed"))?;
		Ok(Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&derived)))
	}

	/// The plaintext, from the environment if it provides one, else the stored row.
	pub async fn get(&self, key: &str) -> ClResult<Option<Vec<u8>>> {
		// Ahead of the cache, not behind it: the cache holds `Option<Vec<u8>>`, so a `None` is
		// cached too and a fallback bolted on afterwards would have to invalidate that entry.
		// The environment cannot change without a restart, so caching it buys nothing.
		if let Some(value) = self.env_value(key) {
			return Ok(Some(value));
		}
		self.row(0, key).await
	}

	/// The plaintext of `key` at `org_id`. Org 0 is [`Self::get`]; any other org reads **its own
	/// row only** — never the environment, never org 0 — so one tenant's missing credential
	/// cannot resolve to the deployment's.
	pub async fn get_at(&self, org_id: i64, key: &str) -> ClResult<Option<Vec<u8>>> {
		match org_id {
			0 => self.get(key).await,
			1.. => self.row(org_id, key).await,
			_ => Err(Error::internal("negative org id for a secret")),
		}
	}

	/// The stored row at `org_id`, through the cache.
	async fn row(&self, org_id: i64, key: &str) -> ClResult<Option<Vec<u8>>> {
		let scoped = scoped(org_id, key);
		let miss = match self.cache.lookup(&scoped) {
			Ok(v) => return Ok(v),
			Err(miss) => miss,
		};
		let plain = match self.store.secret_get(org_id, key).await? {
			Some((nonce, ciphertext)) => {
				if nonce.len() != 12 {
					return Err(Error::internal("stored secret has a malformed nonce"));
				}
				Some(
					self.cipher(&scoped)?
						.decrypt(nonce.as_slice().into(), ciphertext.as_slice())
						.map_err(|_| Error::internal("secret failed to decrypt — wrong MASTER_KEY?"))?,
				)
			}
			None => None,
		};
		self.cache.store(&scoped, miss, plain.clone());
		Ok(plain)
	}

	/// Encrypts and stores `value` under a fresh nonce, replacing any earlier value.
	///
	/// Refused when the environment provides the key: the row would be written and then
	/// shadowed by every later read.
	pub async fn set(&self, key: &str, value: &[u8], updated_by: Option<i64>) -> ClResult<()> {
		if self.env_value(key).is_some() {
			return Err(Error::coded(
				StatusCode::CONFLICT,
				"E-CORE-CONFLICT",
				format!(
					"secret '{key}' comes from {}; change it there",
					crate::settings::env_name(key)
				),
			));
		}
		self.write(0, key, value, updated_by).await
	}

	/// [`Self::set`] at `org_id`. An org above 0 is never shadowed by the environment, so it is
	/// never refused for it.
	pub async fn set_at(
		&self,
		org_id: i64,
		key: &str,
		value: &[u8],
		updated_by: Option<i64>,
	) -> ClResult<()> {
		match org_id {
			0 => self.set(key, value, updated_by).await,
			1.. => self.write(org_id, key, value, updated_by).await,
			_ => Err(Error::internal("negative org id for a secret")),
		}
	}

	async fn write(
		&self,
		org_id: i64,
		key: &str,
		value: &[u8],
		updated_by: Option<i64>,
	) -> ClResult<()> {
		let scoped = scoped(org_id, key);
		let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
		let ciphertext = self
			.cipher(&scoped)?
			.encrypt(&nonce, value)
			.map_err(|_| Error::internal("secret encryption failed"))?;
		self.store
			.secret_set(org_id, key, nonce.as_slice(), &ciphertext, updated_by)
			.await?;
		self.cache.invalidate(&scoped);
		Ok(())
	}

	/// The plaintext, minting `len` random bytes and storing them when the secret is absent.
	///
	/// Concurrent callers converge on whichever value landed first, so a key is **never
	/// replaced once something has signed with it**. That is why `set` is unusable here: two
	/// workers racing on an unseeded `auth.jwt_key` both mint and both store, and the loser's
	/// already-issued sessions die on their next request. `INSERT … DO NOTHING` plus the
	/// re-read is the whole mechanism. An environment-provided key short-circuits at the `get`,
	/// so it is never minted and never stored.
	pub async fn get_or_create(&self, key: &str, len: usize) -> ClResult<Vec<u8>> {
		if let Some(value) = self.get(key).await? {
			return Ok(value);
		}
		let mut fresh = vec![0u8; len];
		aes_gcm::aead::rand_core::RngCore::fill_bytes(&mut OsRng, &mut fresh);
		let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
		let ciphertext = self
			.cipher(key)?
			.encrypt(&nonce, fresh.as_slice())
			.map_err(|_| Error::internal("secret encryption failed"))?;
		self.store.secret_put_if_absent(key, nonce.as_slice(), &ciphertext).await?;
		self.cache.invalidate(key);
		self.get(key).await?.ok_or_else(|| {
			Error::internal(format!("secret `{key}` vanished right after it was minted"))
		})
	}

	/// Whether the secret is set and when it last changed. Never the value.
	pub async fn status(&self, key: &str) -> ClResult<SecretStatus> {
		// No row means no rotation timestamp, and `updated_at: None` says so honestly.
		if self.env_value(key).is_some() {
			return Ok(SecretStatus { set: true, updated_at: None });
		}
		let at = self.store.secret_updated_at(0, key).await?;
		Ok(SecretStatus { set: at.is_some(), updated_at: at })
	}

	/// [`Self::status`] at `org_id`; above 0 only the org's own row counts.
	pub async fn status_at(&self, org_id: i64, key: &str) -> ClResult<SecretStatus> {
		match org_id {
			0 => self.status(key).await,
			1.. => {
				let at = self.store.secret_updated_at(org_id, key).await?;
				Ok(SecretStatus { set: at.is_some(), updated_at: at })
			}
			_ => Err(Error::internal("negative org id for a secret")),
		}
	}
}

/// The HKDF info and cache key of `key` at `org_id`. Org 0 is the bare name, so rows written
/// before secrets were org-scoped still decrypt; the org in the info makes a row moved to
/// another org fail authentication.
fn scoped(org_id: i64, key: &str) -> String {
	if org_id == 0 { key.to_owned() } else { format!("org/{org_id}/{key}") }
}

// vim: ts=4
