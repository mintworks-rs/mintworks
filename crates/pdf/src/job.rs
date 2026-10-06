// SPDX-License-Identifier: MPL-2.0
//! `RENDER_DOC`: render a template directory to a content-addressed PDF under `DATA_DIR`.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use mintworks_core::app::App;
use mintworks_core::error::{ClResult, Error};
use mintworks_core::ids::DocId;
use mintworks_core::job::{Job, Runner};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::store::DocumentStore;

pub const KIND_RENDER_DOC: &str = "RENDER_DOC";

/// The enqueued JSON. The enqueuer picks the `dedup_key` and reads the sha256 back through
/// `CoreStore::job_result_by_key`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Payload {
	/// Template directory, relative to the root given to [`register`].
	dir: String,
	/// Entry file, a key of the loaded set (`"report.typ"`).
	main: String,
	/// Each entry becomes `sys.inputs.<key>`.
	#[serde(default)]
	inputs: BTreeMap<String, String>,
	/// The `documents.uid` to mark rendered, when [`crate::Documents`] enqueued it.
	#[serde(default)]
	doc: Option<String>,
}

/// Registers the `RENDER_DOC` handler; payload directories resolve under `root`. `docs` is
/// required for a payload carrying `doc`, i.e. one [`crate::Documents`] enqueued.
pub fn register(
	runner: &mut Runner,
	app: App,
	root: PathBuf,
	docs: Option<Arc<dyn DocumentStore>>,
) {
	runner.register(KIND_RENDER_DOC, move |job: Job| {
		let (app, root, docs) = (app.clone(), root.clone(), docs.clone());
		async move {
			let p: Payload = serde_json::from_str(&job.payload).map_err(|e| {
				Error::internal(format!("mintworks-pdf: bad {KIND_RENDER_DOC} payload: {e}"))
			})?;
			let doc = p.doc.clone();
			let (sha256, bytes) = run(&app.config.data_dir, &root, p).await?;
			if let Some(uid) = doc {
				let docs = docs.as_ref().ok_or_else(|| {
					Error::internal("mintworks-pdf: RENDER_DOC for a document, no DocumentStore")
				})?;
				docs.document_rendered(&DocId::from_trusted(uid), &sha256, bytes).await?;
			}
			app.store.job_set_result(job.id, &sha256).await
		}
	});
}

/// Normal components only, so a payload path cannot leave the template root.
pub(crate) fn is_contained(rel: &Path) -> bool {
	rel.components().all(|c| matches!(c, Component::Normal(_)))
}

async fn run(data_dir: &str, root: &Path, p: Payload) -> ClResult<(String, i64)> {
	let rel = Path::new(&p.dir);
	if !is_contained(rel) {
		return Err(Error::internal(format!(
			"mintworks-pdf: template dir escapes root: {}",
			p.dir
		)));
	}
	let dir = root.join(rel);
	let data_dir = data_dir.to_owned();
	tokio::task::spawn_blocking(move || -> ClResult<(String, i64)> {
		let files = crate::load_dir(&dir)?;
		let pdf = crate::render(&files, &p.main, &p.inputs, false)?;
		let sha256 = hex::encode(Sha256::digest(&pdf));
		let path = crate::doc_path(&data_dir, &sha256)?;
		if let Some(parent) = path.parent() {
			std::fs::create_dir_all(parent)
				.map_err(|e| Error::Unavailable(format!("mintworks-pdf: mkdir: {e}")))?;
		}
		// Content-addressed: an identical file already there is the same bytes.
		std::fs::write(&path, &pdf)
			.map_err(|e| Error::Unavailable(format!("mintworks-pdf: write: {e}")))?;
		Ok((sha256, i64::try_from(pdf.len()).unwrap_or(i64::MAX)))
	})
	.await
	.map_err(|e| Error::internal(format!("mintworks-pdf: render task: {e}")))?
}

// vim: ts=4
