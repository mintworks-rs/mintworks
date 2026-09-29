//! `Documents`: the service handle. Every method takes `&Ctx` first and is confined to
//! `ctx.org_id`, so another org's document is `E-CORE-NOTFOUND`, never 403.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use saas_core::app::App;
use saas_core::ctx::Ctx;
use saas_core::error::{ClResult, Error, StatusCode};
use saas_core::ids::DocId;
use saas_core::prelude::Timestamp;
use serde::Serialize;

use crate::KIND_RENDER_DOC;
use crate::store::{Document, DocumentStore};

pub const E_PENDING: &str = "E-PDF-PENDING";
pub const E_FAILED: &str = "E-PDF-FAILED";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocView {
	pub uid: DocId,
	/// `PENDING`, `READY` or `FAILED`.
	pub status: &'static str,
	pub sha256: Option<String>,
	pub bytes: Option<i64>,
	pub created_at: Timestamp,
}

#[derive(Clone)]
pub struct Documents {
	app: App,
	store: Arc<dyn DocumentStore>,
}

impl Documents {
	pub fn new(app: App, store: Arc<dyn DocumentStore>) -> Self {
		Self { app, store }
	}

	/// Built over the `Arc<dyn DocumentStore>` an app put in its extensions — the store, not
	/// the handle, because extensions are fixed before an `App` exists.
	pub fn from_app(app: &App) -> ClResult<Self> {
		let store =
			app.extensions.get::<Arc<dyn DocumentStore>>().cloned().ok_or_else(|| {
				Error::internal("saas-pdf: no DocumentStore extension registered")
			})?;
		Ok(Self::new(app.clone(), store))
	}

	/// Queues a render of `template` (relative to the job's template root: its parent dir is
	/// the file set, its file name the entry) with `data` as `sys.inputs.data` (a JSON string).
	pub async fn create(
		&self,
		ctx: &Ctx,
		template: &str,
		data: serde_json::Value,
	) -> ClResult<DocView> {
		let org = ctx.org()?;
		let (dir, main) = split_template(template)?;
		let uid = DocId::generate();
		let job_key = format!("pdf:doc:{uid}", uid = uid.as_str());
		self.store.document_insert(org, &uid, template, &job_key).await?;
		let payload = serde_json::json!({
			"dir": dir,
			"main": main,
			"inputs": { "data": data.to_string() },
			"doc": uid.as_str(),
		});
		let now = Timestamp::now();
		saas_core::job::enqueue(
			&self.app.store,
			KIND_RENDER_DOC,
			&payload.to_string(),
			Some(&job_key),
			now,
		)
		.await?;
		self.get(ctx, uid.as_str()).await
	}

	pub async fn get(&self, ctx: &Ctx, uid: &str) -> ClResult<DocView> {
		let doc = self.doc(ctx, uid).await?;
		let status = self.status(&doc).await?;
		Ok(DocView {
			uid: doc.uid,
			status,
			sha256: doc.sha256,
			bytes: doc.bytes,
			created_at: doc.created_at,
		})
	}

	/// The rendered PDF's bytes.
	pub async fn read(&self, ctx: &Ctx, uid: &str) -> ClResult<Vec<u8>> {
		let doc = self.doc(ctx, uid).await?;
		let Some(sha) = &doc.sha256 else {
			return Err(match self.status(&doc).await? {
				"FAILED" => Error::coded(
					StatusCode::UNPROCESSABLE_ENTITY,
					E_FAILED,
					"the document failed to render",
				),
				_ => Error::coded(
					StatusCode::CONFLICT,
					E_PENDING,
					"the document is not rendered yet",
				),
			});
		};
		let path = crate::doc_path(&self.app.config.data_dir, sha)?;
		tokio::fs::read(&path)
			.await
			.map_err(|e| Error::Unavailable(format!("saas-pdf: reading {}: {e}", path.display())))
	}

	async fn doc(&self, ctx: &Ctx, uid: &str) -> ClResult<Document> {
		let org = ctx.org()?;
		let uid = DocId::parse(uid).map_err(|_| Error::NotFound)?;
		self.store.document_get(org, &uid).await?.ok_or(Error::NotFound)
	}

	async fn status(&self, doc: &Document) -> ClResult<&'static str> {
		if doc.sha256.is_some() {
			return Ok("READY");
		}
		// No row yet counts as pending: a keyed job row is never swept.
		Ok(match self.app.store.job_result_by_key(&doc.job_key).await? {
			Some((status, _)) if status == "FAILED" => "FAILED",
			_ => "PENDING",
		})
	}
}

/// `a/b/x.typ` → (`a/b`, `x.typ`), checked here so a bad path fails the call, not the job.
fn split_template(template: &str) -> ClResult<(String, String)> {
	let path = Path::new(template);
	let bad = || Error::validation(format!("bad template path: {template}"));
	if !crate::job::is_contained(path) {
		return Err(bad());
	}
	let main = path.file_name().and_then(|n| n.to_str()).ok_or_else(bad)?;
	let dir = path.parent().map(PathBuf::from).unwrap_or_default();
	Ok((dir.to_str().ok_or_else(bad)?.to_owned(), main.to_owned()))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn template_splits_into_dir_and_main() {
		assert_eq!(
			split_template("templates/demo.typ").unwrap(),
			("templates".into(), "demo.typ".into())
		);
		assert_eq!(split_template("demo.typ").unwrap(), (String::new(), "demo.typ".into()));
		for bad in ["../x.typ", "/etc/x.typ", "a/../x.typ", ""] {
			assert!(split_template(bad).is_err(), "{bad}");
		}
	}
}

// vim: ts=4
