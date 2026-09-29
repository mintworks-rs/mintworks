//! Typst document rendering shared by invoices and app documents: a [`Files`] template set
//! (with `#import` between its files) plus string `sys.inputs`, compiled to PDF (or PDF/A-3b).
//!
//! Typst does no arithmetic on money: amounts reach a template as pre-formatted strings.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use saas_core::error::{ClResult, Error};
use typst::syntax::VirtualPath;

mod hook;
mod job;
mod markdown;
pub mod routes;
pub mod service;
pub mod store;
mod world;

pub use hook::DocumentHook;
pub use job::{KIND_RENDER_DOC, register};
pub use markdown::to_typst;
pub use routes::routes;
pub use service::{DocView, Documents};
pub use store::{Document, DocumentStore};
pub use world::Files;

/// Compiles `main` (a key of `files`) against `inputs` and returns PDF bytes. Pure and
/// blocking — call it from `spawn_blocking`, never straight off an async task.
///
/// `pdfa` exports PDF/A-3b, which requires the template to `#set document(date: …)`:
/// `World::today` is `none`, so there is no fallback date.
pub fn render(
	files: &Files,
	main: &str,
	inputs: &BTreeMap<String, String>,
	pdfa: bool,
) -> ClResult<Vec<u8>> {
	let world = world::DocWorld::new(files, main, inputs)?;
	let doc = typst::compile(&world).output.map_err(|errs| {
		let first = errs.first().map(|e| e.message.to_string()).unwrap_or_default();
		Error::internal(format!("saas-pdf: typst compile failed: {first}"))
	})?;
	// PDF/A-3b: the archived file is the statutory evidence copy, and A-3 is the one archival
	// level that permits embedded files, leaving the Factur-X XML attachment open later.
	let wanted: &[typst_pdf::PdfStandard] =
		if pdfa { &[typst_pdf::PdfStandard::A_3b] } else { &[] };
	let standards = typst_pdf::PdfStandards::new(wanted)
		.map_err(|e| Error::internal(format!("saas-pdf: PDF/A-3b: {}", e.message())))?;
	typst_pdf::pdf(&doc, &typst_pdf::PdfOptions { standards, ..Default::default() }).map_err(
		|errs| {
			let first = errs.first().map(|e| e.message.to_string()).unwrap_or_default();
			Error::internal(format!("saas-pdf: typst pdf export failed: {first}"))
		},
	)
}

/// Reads every regular file under `dir`, recursively, into a [`Files`] keyed by its path
/// relative to `dir`. Symlinks are skipped: one could point outside the template set.
pub fn load_dir(dir: &Path) -> ClResult<Files> {
	let mut files = Files::new();
	let mut stack = vec![dir.to_path_buf()];
	while let Some(d) = stack.pop() {
		let entries = std::fs::read_dir(&d)
			.map_err(|e| Error::internal(format!("saas-pdf: read {}: {e}", d.display())))?;
		for entry in entries {
			let path = entry
				.map_err(|e| Error::internal(format!("saas-pdf: read {}: {e}", d.display())))?
				.path();
			let meta = std::fs::symlink_metadata(&path)
				.map_err(|e| Error::internal(format!("saas-pdf: stat {}: {e}", path.display())))?;
			if meta.is_dir() {
				stack.push(path);
			} else if meta.is_file() {
				let key = VirtualPath::virtualize(dir, &path)
					.map_err(|e| Error::internal(format!("saas-pdf: {}: {e:?}", path.display())))?
					.get_without_slash()
					.to_owned();
				let bytes = std::fs::read(&path).map_err(|e| {
					Error::internal(format!("saas-pdf: read {}: {e}", path.display()))
				})?;
				files.insert(key, bytes);
			}
		}
	}
	Ok(files)
}

/// `{data_dir}/documents/{sha[0..2]}/{sha[2..4]}/{sha}.pdf`. Content-addressed, so the
/// stored hash is both the location and the integrity check and there is no path column.
pub fn doc_path(data_dir: &str, sha256: &str) -> ClResult<PathBuf> {
	let (a, b) = sha256
		.get(0..2)
		.zip(sha256.get(2..4))
		.ok_or_else(|| Error::internal("saas-pdf: short sha256"))?;
	Ok(PathBuf::from(data_dir)
		.join("documents")
		.join(a)
		.join(b)
		.join(format!("{sha256}.pdf")))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn doc_path_fans_out_on_the_hash() {
		let p = doc_path("/data", "abcdef").unwrap();
		assert_eq!(p, PathBuf::from("/data/documents/ab/cd/abcdef.pdf"));
		assert!(doc_path("/data", "abc").is_err());
	}

	#[test]
	fn imports_resolve_within_the_set() {
		let mut files = Files::new();
		files.insert(
			"main.typ".into(),
			b"#import \"parts/x.typ\": hi\n#hi #sys.inputs.name".to_vec(),
		);
		files.insert("parts/x.typ".into(), b"#let hi = [Hello]".to_vec());
		let inputs = BTreeMap::from([("name".to_owned(), "world".to_owned())]);
		let pdf = render(&files, "main.typ", &inputs, false).unwrap();
		assert!(pdf.starts_with(b"%PDF"));
	}

	#[test]
	fn packages_and_missing_files_are_unreachable() {
		for src in ["#import \"@preview/foo:0.1.0\": *", "#include \"nope.typ\""] {
			let files = Files::from([("main.typ".to_owned(), src.as_bytes().to_vec())]);
			assert!(render(&files, "main.typ", &BTreeMap::new(), false).is_err());
		}
	}
}

// vim: ts=4
