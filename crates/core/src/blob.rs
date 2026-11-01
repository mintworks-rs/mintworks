// SPDX-License-Identifier: MPL-2.0
//! Content-addressed uploads: the bytes on disk by sha256, one `blobs` row per upload and one
//! `blob_refs` row per holder. A blob no holder claims is removed by [`KIND_GC`] once it has
//! been unreferenced for `blob.unclaimed_ttl_secs`.
//!
//! The framework confines a blob to the caller's orgs and a hold's release to the account that
//! claimed it (or an org `ADMIN`); who may claim or read it is the application's decision, as
//! with `fs::open`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{Bytes, HttpBody};
use axum::http::StatusCode;
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::account_data::AccountDataHook;
use crate::app::App;
use crate::auth_mw::member_role_on;
use crate::ctx::Ctx;
use crate::error::{ClResult, Error};
use crate::ids::{AccountId, BlobId};
use crate::job::{self, Runner};
use crate::settings::SettingDef;
use crate::store::{Blob, CoreStore, Role};
use crate::types::Timestamp;

/// The hourly sweep of unreferenced blobs.
pub const KIND_GC: &str = "BLOB_GC";

const SWEEP_BATCH: i64 = 500;
const MAX_LABEL: usize = 255;

pub static SETTINGS: &[SettingDef] = &[
	// Covers an abandoned upload and a release followed by a re-claim alike: Rune's claim and
	// release apply on the core DB's commit, never in the app DB's transaction.
	SettingDef::int(
		"blob.unclaimed_ttl_secs",
		"86400",
		"Seconds an unclaimed or released blob is kept before BLOB_GC deletes it.",
	)
	.range(60, 31_536_000),
	SettingDef::int(
		"blob.max_org_bytes",
		"1073741824",
		"Bytes of blobs one org may hold, claimed or not, before put is refused.",
	)
	.range(1_048_576, i64::MAX),
];

/// A scratch file older than this is an interrupted write or bury, removed by `gc`.
const TMP_MAX_AGE: std::time::Duration = std::time::Duration::from_hours(1);

/// `{data_dir}/{dir}/{sha[0..2]}/{sha[2..4]}/{sha}{ext}`, the content-addressed layout shared by
/// `blobs/` and `mintworks_pdf`'s `documents/`.
pub fn sha_path(data_dir: &str, dir: &str, sha256: &str, ext: &str) -> ClResult<PathBuf> {
	let (a, b) = sha256
		.get(0..2)
		.zip(sha256.get(2..4))
		.ok_or_else(|| Error::internal("short sha256"))?;
	Ok(PathBuf::from(data_dir).join(dir).join(a).join(b).join(format!("{sha256}{ext}")))
}

pub fn blob_path(data_dir: &str, sha256: &str) -> ClResult<PathBuf> {
	sha_path(data_dir, "blobs", sha256, "")
}

/// `{data_dir}/blobs/tmp`: three characters, so no sha fan-out dir; same filesystem, so a
/// rename out of it is atomic.
fn tmp_dir(data_dir: &str) -> PathBuf {
	PathBuf::from(data_dir).join("blobs").join("tmp")
}

fn tmp_path(data_dir: &str) -> std::io::Result<PathBuf> {
	let dir = tmp_dir(data_dir);
	std::fs::create_dir_all(&dir)?;
	Ok(dir.join(ulid::Ulid::generate().to_string()))
}

/// A body spooled to the scratch dir by [`spool`], hashed and counted; the file is removed on
/// drop unless [`Blobs::put_upload`] placed it.
pub struct Upload {
	path: Option<PathBuf>,
	pub sha256: String,
	pub size: i64,
}

impl Drop for Upload {
	fn drop(&mut self) {
		if let Some(p) = self.path.take() {
			let _ = std::fs::remove_file(p);
		}
	}
}

/// Why [`spool`] failed: the body itself (a `LengthLimitError` among them), or the disk.
#[derive(Debug)]
pub enum SpoolError {
	Body(axum::BoxError),
	Io(Error),
}

/// Streams `body` into the scratch dir, hashing as it goes. No limit of its own: wrap the body
/// in `http_body_util::Limited`.
pub async fn spool<B>(data_dir: &str, body: B) -> Result<Upload, SpoolError>
where
	B: HttpBody<Data = Bytes>,
	B::Error: Into<axum::BoxError>,
{
	use tokio::io::AsyncWriteExt as _;
	let io = |e: std::io::Error| SpoolError::Io(Error::Unavailable(format!("blob: spool: {e}")));
	let path = tmp_path(data_dir).map_err(io)?;
	let mut file = tokio::fs::File::create(&path).await.map_err(io)?;
	// Owns the file from here, so every early return below removes it.
	let mut up = Upload { path: Some(path), sha256: String::new(), size: 0 };
	let (mut hasher, mut body) = (Sha256::new(), std::pin::pin!(body));
	while let Some(frame) = body.frame().await {
		let frame = frame.map_err(|e| SpoolError::Body(e.into()))?;
		if let Ok(chunk) = frame.into_data() {
			hasher.update(&chunk);
			up.size = up.size.saturating_add(i64::try_from(chunk.len()).unwrap_or(i64::MAX));
			file.write_all(&chunk).await.map_err(io)?;
		}
	}
	file.flush().await.map_err(io)?;
	file.sync_all().await.map_err(io)?;
	up.sha256 = hex::encode(hasher.finalize());
	Ok(up)
}

/// Moves `sha256`'s file to `blobs/tmp/<sha>.tomb`, the name [`recover_tombs`] reads the sha
/// back from; `None` if it was already absent.
fn bury(data_dir: &str, sha256: &str) -> std::io::Result<Option<(PathBuf, PathBuf)>> {
	let dir = tmp_dir(data_dir);
	std::fs::create_dir_all(&dir)?;
	let (path, tomb) = (
		blob_path(data_dir, sha256).map_err(std::io::Error::other)?,
		dir.join(format!("{sha256}.tomb")),
	);
	match std::fs::rename(&path, &tomb) {
		Ok(()) => Ok(Some((path, tomb))),
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
		Err(e) => Err(e),
	}
}

/// Puts a buried file back.
fn restore(path: &std::path::Path, tomb: &std::path::Path) -> std::io::Result<()> {
	std::fs::rename(tomb, path)
}

/// Deletes a buried file for good.
fn discard(tomb: &std::path::Path) -> std::io::Result<()> {
	std::fs::remove_file(tomb)
}

/// The tmp dir's entries; a missing dir is none.
fn tmp_entries(data_dir: &str) -> std::io::Result<Vec<std::fs::DirEntry>> {
	match std::fs::read_dir(tmp_dir(data_dir)) {
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
		r => r?.collect(),
	}
}

fn is_tomb(entry: &std::fs::DirEntry) -> bool {
	entry.file_name().to_string_lossy().ends_with(".tomb")
}

/// Settles tombs a crashed or concurrent `gc` left: restored if the sha is (or may be) in use,
/// deleted otherwise. A tomb another instance settles first is a `NotFound`, ignored.
async fn recover_tombs(app: &App) -> ClResult<()> {
	let data_dir = &app.config.data_dir;
	let entries =
		tmp_entries(data_dir).map_err(|e| Error::Unavailable(format!("blob: tmp: {e}")))?;
	for tomb in entries.iter().filter(|e| is_tomb(e)).map(std::fs::DirEntry::path) {
		let Some(sha) = tomb.file_stem().and_then(|s| s.to_str()) else { continue };
		let Ok(path) = blob_path(data_dir, sha) else { continue };
		let in_use = !matches!(app.store.blob_sha_in_use(sha).await, Ok(false));
		match if in_use { restore(&path, &tomb) } else { discard(&tomb) } {
			Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
				tracing::warn!(sha, in_use, error = %e, "blob tomb recovery failed");
			}
			_ => {}
		}
	}
	Ok(())
}

/// Removes scratch files older than [`TMP_MAX_AGE`]; a tomb is [`recover_tombs`]'s, never aged
/// out, since `rename` keeps the buried file's old mtime.
fn clean_tmp(data_dir: &str, max_age: std::time::Duration) -> std::io::Result<()> {
	for entry in tmp_entries(data_dir)? {
		if is_tomb(&entry) {
			continue;
		}
		let age = entry.metadata()?.modified()?.elapsed().unwrap_or_default();
		if age > max_age {
			let _ = std::fs::remove_file(entry.path());
		}
	}
	Ok(())
}

/// The blob service handle. Every method confines to the orgs `ctx` belongs to: a blob of
/// another org is `E-CORE-NOTFOUND`. `System` reaches every org.
#[derive(Clone)]
pub struct Blobs {
	app: App,
}

impl Blobs {
	pub fn new(app: &App) -> Self {
		Self { app: app.clone() }
	}

	/// Stores `bytes` as a new, unclaimed blob of `org_id`; [`Self::put_upload`] for a body.
	pub async fn put(
		&self,
		ctx: &Ctx,
		org_id: i64,
		bytes: Bytes,
		mime: &str,
		name: Option<&str>,
	) -> ClResult<BlobId> {
		let upload = spool(&self.app.config.data_dir, http_body_util::Full::new(bytes))
			.await
			.map_err(|e| match e {
				SpoolError::Io(e) => e,
				SpoolError::Body(e) => Error::internal(format!("blob: {e}")),
			})?;
		self.put_upload(ctx, org_id, upload, mime, name).await
	}

	/// Stores a [`spool`]ed body as a new, unclaimed blob of `org_id` and returns its uid. Unless
	/// claimed within `blob.unclaimed_ttl_secs`, [`KIND_GC`] removes it.
	pub async fn put_upload(
		&self,
		ctx: &Ctx,
		org_id: i64,
		mut upload: Upload,
		mime: &str,
		name: Option<&str>,
	) -> ClResult<BlobId> {
		label("mime", mime)?;
		if let Some(n) = name {
			label("name", n)?;
		}
		member_role_on(&self.app, ctx, org_id).await?;
		let cap = self.app.settings.int("blob.max_org_bytes").await?;
		let (uid, now) = (BlobId::generate(), Timestamp::now());
		let row = Blob {
			id: 0,
			uid: uid.clone(),
			org_id,
			sha256: upload.sha256.clone(),
			mime: mime.to_owned(),
			size: upload.size,
			name: name.map(str::to_owned),
			created_by: ctx.actor.account_id(),
			created_at: now,
			unref_at: Some(now),
		};
		// Row first, so `gc` sees the sha in use before the file exists; a failed write deletes
		// it again, so it never counts toward the quota.
		let Some(row_id) = self.app.store.blob_insert(&row, cap).await? else {
			let msg = "this organisation's uploads are over quota";
			return Err(Error::coded(StatusCode::PAYLOAD_TOO_LARGE, "E-CORE-QUOTA", msg));
		};
		let path = blob_path(&self.app.config.data_dir, &row.sha256)?;
		let size = upload.size;
		let written = tokio::task::spawn_blocking(move || -> ClResult<()> {
			// Same size: the bytes are already there and `upload`'s drop removes the duplicate.
			// A different size is a crash-truncated file, which the rename below replaces.
			if std::fs::metadata(&path).is_ok_and(|m| i64::try_from(m.len()).ok() == Some(size)) {
				return Ok(());
			}
			if let Some(parent) = path.parent() {
				std::fs::create_dir_all(parent)
					.map_err(|e| Error::Unavailable(format!("blob: mkdir: {e}")))?;
			}
			let Some(tmp) = upload.path.take() else { return Ok(()) };
			std::fs::rename(&tmp, &path).map_err(|e| {
				let _ = std::fs::remove_file(&tmp);
				Error::Unavailable(format!("blob: write: {e}"))
			})
		})
		.await
		.map_err(|e| Error::internal(format!("blob: write task: {e}")))
		.and_then(|r| r);
		if let Err(e) = written {
			if let Err(d) = self.app.store.blob_delete(row_id).await {
				tracing::warn!(blob = uid.as_str(), error = %d, "blob row left after failed write");
			}
			return Err(e);
		}
		Ok(uid)
	}

	pub async fn get(&self, ctx: &Ctx, uid: &str) -> ClResult<Blob> {
		Ok(self.find(ctx, uid).await?.0)
	}

	/// The blob and `ctx`'s role on its org, `None` for `System`.
	async fn find(&self, ctx: &Ctx, uid: &str) -> ClResult<(Blob, Option<Role>)> {
		let uid = BlobId::parse(uid).map_err(|_| Error::NotFound)?;
		let blob = self.app.store.blob_get(&uid).await?.ok_or(Error::NotFound)?;
		let role = member_role_on(&self.app, ctx, blob.org_id).await?;
		Ok((blob, role))
	}

	/// Idempotent per `holder`. A `holder` another account already claims is
	/// `E-AUTH-FORBIDDEN` for a caller below `ADMIN`, who could not release it either.
	pub async fn claim(&self, ctx: &Ctx, uid: &str, holder: &str) -> ClResult<()> {
		label("ref", holder)?;
		let (blob, by) = self.hold_scope(ctx, uid).await?;
		let owner = self
			.app
			.store
			.blob_claim(blob.id, holder, ctx.actor.account_id(), Timestamp::now())
			.await?;
		if by.is_some() && owner != by {
			return Err(another_hold());
		}
		Ok(())
	}

	/// The blob, and the account whose holds `ctx` is confined to: its own below `ADMIN`,
	/// `None` (every hold) for an `ADMIN` or `System`.
	async fn hold_scope(&self, ctx: &Ctx, uid: &str) -> ClResult<(Blob, Option<i64>)> {
		let (blob, role) = self.find(ctx, uid).await?;
		let by = match role {
			Some(r) if r < Role::Admin => ctx.actor.account_id(),
			_ => None,
		};
		Ok((blob, by))
	}

	/// Idempotent per `holder`; the last release starts the grace period. Only the claiming
	/// account, an `ADMIN` of the blob's org or `System` releases a hold; anyone else is
	/// `E-AUTH-FORBIDDEN`.
	pub async fn release(&self, ctx: &Ctx, uid: &str, holder: &str) -> ClResult<()> {
		label("ref", holder)?;
		let (blob, by) = self.hold_scope(ctx, uid).await?;
		let gone = self.app.store.blob_release(blob.id, holder, by, Timestamp::now()).await?;
		if !gone
			&& by.is_some()
			&& self.app.store.blob_refs(blob.id).await?.iter().any(|r| r == holder)
		{
			return Err(another_hold());
		}
		Ok(())
	}

	pub async fn refs(&self, ctx: &Ctx, uid: &str) -> ClResult<Vec<String>> {
		let blob = self.get(ctx, uid).await?;
		self.app.store.blob_refs(blob.id).await
	}

	/// The row and the path of its bytes.
	pub async fn open(&self, ctx: &Ctx, uid: &str) -> ClResult<(Blob, PathBuf)> {
		let blob = self.get(ctx, uid).await?;
		let path = blob_path(&self.app.config.data_dir, &blob.sha256)?;
		Ok((blob, path))
	}
}

fn another_hold() -> Error {
	Error::coded(StatusCode::FORBIDDEN, "E-AUTH-FORBIDDEN", "another account's hold")
}

fn label(what: &str, v: &str) -> ClResult<()> {
	if v.is_empty() || v.len() > MAX_LABEL {
		return Err(Error::Validation(format!("blob {what} must be 1..={MAX_LABEL} bytes")));
	}
	Ok(())
}

/// Installs [`KIND_GC`]; seed it with [`seed`].
pub fn register(runner: &mut Runner, app: App) {
	runner.register_periodic(KIND_GC, 3_600, move |_job| {
		let app = app.clone();
		async move { gc(&app).await }
	});
}

/// Seeds the periodic sweep. Call once at boot.
pub async fn seed(store: &Arc<dyn CoreStore>) -> ClResult<()> {
	job::seed_periodic(store, KIND_GC).await
}

/// Deletes `sha`'s file unless a row still names it.
async fn reap(app: &App, sha: &str) -> ClResult<()> {
	if app.store.blob_sha_in_use(sha).await? {
		return Ok(());
	}
	// `put` inserts its row before checking the file: a row landing before the re-check
	// restores the file, one landing after finds it missing and writes it again.
	let buried = match bury(&app.config.data_dir, sha) {
		Ok(Some(b)) => b,
		Ok(None) => return Ok(()),
		Err(e) => {
			tracing::warn!(sha, error = %e, "blob file left behind");
			return Ok(());
		}
	};
	// A failed re-check restores the file: unknown means in use, never delete.
	let in_use = app.store.blob_sha_in_use(sha).await;
	let keep = !matches!(in_use, Ok(false));
	let (path, tomb) = &buried;
	if let Err(e) = if keep { restore(path, tomb) } else { discard(tomb) } {
		tracing::warn!(sha, keep, error = %e, "blob unbury failed");
	}
	in_use.map(|_| ())
}

/// The shas of `blobs/??/??/*` files last modified more than `max_age` ago; `tmp/` is skipped.
fn old_blob_files(data_dir: &str, max_age: std::time::Duration) -> std::io::Result<Vec<String>> {
	let read = |p: &std::path::Path| match std::fs::read_dir(p) {
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
		r => r?.collect::<std::io::Result<Vec<_>>>(),
	};
	let mut out = Vec::new();
	for a in read(&PathBuf::from(data_dir).join("blobs"))? {
		if a.file_name().len() != 2 || !a.file_type()?.is_dir() {
			continue;
		}
		for b in read(&a.path())? {
			if !b.file_type()?.is_dir() {
				continue;
			}
			for f in read(&b.path())? {
				let age = f.metadata()?.modified()?.elapsed().unwrap_or_default();
				if age > max_age
					&& let Some(sha) = f.file_name().to_str()
				{
					out.push(sha.to_owned());
				}
			}
		}
	}
	Ok(out)
}

/// One sweep pass — the [`KIND_GC`] job body.
///
/// # Errors
/// A store failure; a file that cannot be removed is logged and left.
pub async fn gc(app: &App) -> ClResult<()> {
	let ttl = app.settings.int("blob.unclaimed_ttl_secs").await?;
	let cutoff = Timestamp(Timestamp::now().0.saturating_sub(ttl));
	let data_dir = &app.config.data_dir;
	loop {
		let shas = app.store.blob_sweep(cutoff, SWEEP_BATCH).await?;
		for sha in &shas {
			reap(app, sha).await?;
		}
		if i64::try_from(shas.len()).unwrap_or(i64::MAX) < SWEEP_BATCH {
			break;
		}
	}
	// Rows can vanish without the sweep seeing their sha: an org's cascade, GDPR erasure, a
	// crash mid-sweep. Their files are found here instead.
	let (dir, age) = (data_dir.clone(), std::time::Duration::from_secs(ttl.unsigned_abs()));
	let old = tokio::task::spawn_blocking(move || old_blob_files(&dir, age))
		.await
		.map_err(|e| Error::internal(format!("blob: scan task: {e}")))?;
	match old {
		// One in-use query per old file each run; batch the lookup if the store grows large
		Ok(shas) => {
			for sha in &shas {
				reap(app, sha).await?;
			}
		}
		Err(e) => tracing::warn!(error = %e, "blob tree scan failed"),
	}
	recover_tombs(app).await?;
	let dir = data_dir.clone();
	match tokio::task::spawn_blocking(move || clean_tmp(&dir, TMP_MAX_AGE)).await {
		Ok(Err(e)) => tracing::warn!(error = %e, "blob tmp cleanup failed"),
		Err(e) => tracing::warn!(error = %e, "blob tmp cleanup task failed"),
		Ok(Ok(())) => {}
	}
	Ok(())
}

/// GDPR: lists the blobs an account uploaded. Erasure deletes the blobs of its personal org; a
/// blob of any other org is that org's, so it stays but loses the name the uploader gave it.
pub struct BlobHook {
	pub store: Arc<dyn CoreStore>,
}

#[async_trait]
impl AccountDataHook for BlobHook {
	fn name(&self) -> &'static str {
		"blobs"
	}

	async fn export(&self, acc: &AccountId) -> ClResult<Value> {
		let blobs: Vec<Value> = self
			.store
			.blobs_by_creator(acc)
			.await?
			.into_iter()
			.map(|b| {
				json!({
					"uid": b.uid.as_str(),
					"sha256": b.sha256,
					"mime": b.mime,
					"size": b.size,
					"name": b.name,
					"createdAt": b.created_at,
				})
			})
			.collect();
		Ok(json!(blobs))
	}

	async fn erase(&self, acc: &AccountId) -> ClResult<()> {
		self.store.blobs_erase(acc).await
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn blob_path_fans_out_on_the_hash() {
		let p = blob_path("/data", "abcdef").unwrap();
		assert_eq!(p, PathBuf::from("/data/blobs/ab/cd/abcdef"));
		assert!(blob_path("/data", "abc").is_err());
	}

	#[test]
	fn bury_restores_in_use_and_removes_unused() {
		let dir = std::env::temp_dir().join(format!("mw-blob-{}", ulid::Ulid::generate()));
		let d = dir.to_str().unwrap();
		let path = blob_path(d, "abcdef").unwrap();
		std::fs::create_dir_all(path.parent().unwrap()).unwrap();
		std::fs::write(&path, b"x").unwrap();
		let (p, tomb) = bury(d, "abcdef").unwrap().unwrap();
		assert!(!path.exists());
		restore(&p, &tomb).unwrap();
		assert_eq!(std::fs::read(&path).unwrap(), b"x");
		let (_, tomb) = bury(d, "abcdef").unwrap().unwrap();
		discard(&tomb).unwrap();
		assert!(!path.exists() && !tomb.exists());
		assert!(bury(d, "abcdef").unwrap().is_none());
		// A tomb is never aged out: only `recover_tombs` settles it.
		std::fs::write(tmp_dir(d).join("abcdef.tomb"), b"x").unwrap();
		// A fresh scratch file survives the cleanup; a zero max age removes it.
		std::fs::write(tmp_path(d).unwrap(), b"y").unwrap();
		clean_tmp(d, TMP_MAX_AGE).unwrap();
		assert_eq!(std::fs::read_dir(tmp_dir(d)).unwrap().count(), 2);
		std::thread::sleep(std::time::Duration::from_millis(20));
		clean_tmp(d, std::time::Duration::ZERO).unwrap();
		let left: Vec<_> =
			std::fs::read_dir(tmp_dir(d)).unwrap().map(|e| e.unwrap().file_name()).collect();
		assert_eq!(left, ["abcdef.tomb"]);
		std::fs::remove_dir_all(&dir).unwrap();
		clean_tmp(d, TMP_MAX_AGE).unwrap();
	}
}

// vim: ts=4
