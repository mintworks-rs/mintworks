// SPDX-License-Identifier: MPL-2.0
//! `blob::` — `mintworks_core::blob::Blobs` for script: store a `.raw()` route's body, claim and
//! release it per holder, list its holders, and open it for `resp::stream`. Ids are `Ref<str>`, not
//! `String`: a `String` parameter is taken, emptying the caller's local.

use std::sync::{Arc, Mutex, PoisonError};

use mintworks_core::blob::{Blobs, Upload};
use rune::{Any, ContextError, Module, Value, runtime::Ref};

use crate::{
	ScriptCtx,
	error::{R, bad},
	stream::FileHandle,
	tx,
	value::{ScriptError, org_id, to_json},
};

/// `req.body` on a `.raw()` route: the spooled body, opaque to the script. One `blob::put` takes
/// it; an untaken one is deleted with the request.
#[derive(Any)]
pub struct RequestBody {
	upload: Arc<Mutex<Option<Upload>>>,
}

impl RequestBody {
	pub(crate) fn new(upload: Upload) -> Self {
		Self { upload: Arc::new(Mutex::new(Some(upload))) }
	}
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PutOpts {
	org: String,
	mime: String,
	#[serde(default)]
	name: Option<String>,
}

/// `blob::put(ctx, req.body, #{org, mime, name?})` — a new unclaimed blob of `org`, as `blb_…`.
#[rune::function]
async fn put(c: Ref<ScriptCtx>, body: Ref<RequestBody>, opts: Value) -> R<String> {
	tx::outside_tx("blob::put")?;
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let slot = Arc::clone(&body.upload);
	drop(body);
	// Arguments first: a bad call leaves the body for a corrected retry.
	let o: PutOpts = serde_json::from_value(to_json(&opts).map_err(ScriptError)?)
		.map_err(|e| bad(format!("blob::put: {e}")))?;
	let org = org_id(&app, &o.org).await?;
	let upload = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
	let upload = upload.ok_or_else(|| bad("blob::put: body already stored"))?;
	let uid = Blobs::new(&app)
		.put_upload(&ctx, org, upload, &o.mime, o.name.as_deref())
		.await
		.map_err(ScriptError)?;
	Ok(uid.to_string())
}

/// `blob::claim(ctx, uid, ref)` — idempotent per `ref`; joins an open `tx::with`.
#[rune::function]
async fn claim(c: Ref<ScriptCtx>, uid: Ref<str>, holder: Ref<str>) -> R<()> {
	tx::outside_app_tx("blob::claim")?;
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	Blobs::new(&app).claim(&ctx, &uid, &holder).await.map_err(ScriptError)
}

/// `blob::release(ctx, uid, ref)` — the last release starts `blob.unclaimed_ttl_secs`.
#[rune::function]
async fn release(c: Ref<ScriptCtx>, uid: Ref<str>, holder: Ref<str>) -> R<()> {
	tx::outside_app_tx("blob::release")?;
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	Blobs::new(&app).release(&ctx, &uid, &holder).await.map_err(ScriptError)
}

/// `blob::refs(ctx, uid)` — the holders, as `[ref]`.
#[rune::function]
async fn refs(c: Ref<ScriptCtx>, uid: Ref<str>) -> R<Vec<String>> {
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	Blobs::new(&app).refs(&ctx, &uid).await.map_err(ScriptError)
}

/// `blob::open(ctx, uid)` — a handle `resp::stream` serves with the blob's own mime.
#[rune::function]
async fn open(c: Ref<ScriptCtx>, uid: Ref<str>) -> R<FileHandle> {
	let (app, ctx) = (c.app()?.clone(), c.ctx().clone());
	drop(c);
	let (blob, path) = Blobs::new(&app).open(&ctx, &uid).await.map_err(ScriptError)?;
	Ok(FileHandle { path, mime: Some(blob.mime), name: blob.name, attachment: true })
}

/// # Errors
/// Whatever Rune raises registering a type or a function.
pub fn module() -> Result<Module, ContextError> {
	let mut m = Module::with_item(["blob"])?;
	m.ty::<RequestBody>()?;
	m.function_meta(put)?;
	m.function_meta(claim)?;
	m.function_meta(release)?;
	m.function_meta(refs)?;
	m.function_meta(open)?;
	Ok(m)
}

// vim: ts=4
