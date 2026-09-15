//! The `secrets` table: AES-256-GCM ciphertext under `HKDF(MASTER_KEY, key)`.
//!
//! Deriving a distinct key per secret name means a row moved to another name fails
//! authentication rather than decrypting, so the name is bound to the ciphertext without
//! a separate AAD.
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
	error::{ClResult, Error},
	gencache::GenCache,
	store::CoreStore,
	types::Timestamp,
};

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
}

impl SecretStore {
	pub fn new(store: Arc<dyn CoreStore>, master_key: [u8; 32]) -> Self {
		Self { store, master_key, cache: GenCache::new() }
	}

	fn cipher(&self, key: &str) -> ClResult<Aes256Gcm> {
		let mut derived = [0u8; 32];
		Hkdf::<Sha256>::new(None, &self.master_key)
			.expand(key.as_bytes(), &mut derived)
			.map_err(|_| Error::internal("secret key derivation failed"))?;
		Ok(Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&derived)))
	}

	/// The plaintext, or `None` when the secret has never been set.
	pub async fn get(&self, key: &str) -> ClResult<Option<Vec<u8>>> {
		let miss = match self.cache.lookup(key) {
			Ok(v) => return Ok(v),
			Err(miss) => miss,
		};
		let plain = match self.store.secret_get(key).await? {
			Some((nonce, ciphertext)) => {
				if nonce.len() != 12 {
					return Err(Error::internal("stored secret has a malformed nonce"));
				}
				Some(
					self.cipher(key)?
						.decrypt(nonce.as_slice().into(), ciphertext.as_slice())
						.map_err(|_| {
							Error::internal("secret failed to decrypt — wrong MASTER_KEY?")
						})?,
				)
			}
			None => None,
		};
		self.cache.store(key, miss, plain.clone());
		Ok(plain)
	}

	/// Encrypts and stores `value` under a fresh nonce, replacing any earlier value.
	pub async fn set(&self, key: &str, value: &[u8], updated_by: Option<i64>) -> ClResult<()> {
		let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
		let ciphertext = self
			.cipher(key)?
			.encrypt(&nonce, value)
			.map_err(|_| Error::internal("secret encryption failed"))?;
		self.store.secret_set(key, nonce.as_slice(), &ciphertext, updated_by).await?;
		self.cache.invalidate(key);
		Ok(())
	}

	/// The plaintext, minting `len` random bytes and storing them when the secret is absent.
	///
	/// Concurrent callers converge on whichever value landed first, so a key is **never
	/// replaced once something has signed with it**. That is why `set` is unusable here: two
	/// workers racing on an unseeded `auth.jwt_key` both mint and both store, and the loser's
	/// already-issued sessions die on their next request. `INSERT … DO NOTHING` plus the
	/// re-read is the whole mechanism.
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
		let at = self.store.secret_updated_at(key).await?;
		Ok(SecretStatus { set: at.is_some(), updated_at: at })
	}
}

// vim: ts=4
