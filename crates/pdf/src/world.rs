//! A typst [`World`] over an in-memory file set and the fonts bundled in `typst-assets`.
//!
//! Nothing outside the set is reachable: no disk, no network, no package registry. A file
//! under a package root (`@preview/…`) is `NotFound`, the same as a missing one.

use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

use mintworks_core::error::{ClResult, Error};
use typst::diag::{FileError, FileResult};
use typst::foundations::{Bytes, Datetime, Dict, Duration, Value};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{
	Library, LibraryExt,
	syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot},
};

/// A template set: path within the project, without the leading slash (`"invoice.typ"`,
/// `"parts/header.typ"`), to the file's bytes.
pub type Files = BTreeMap<String, Vec<u8>>;

/// The bundled faces are the same on every render, and parsing the ~10 MB of `typst-assets`
/// font bytes per job dominated the render itself.
static FONTS: LazyLock<(LazyHash<FontBook>, Vec<Font>)> = LazyLock::new(|| {
	let mut book = FontBook::new();
	let mut fonts = Vec::new();
	for bytes in typst_assets::fonts() {
		for font in Font::iter(Bytes::new(bytes)) {
			book.push(font.info().clone());
			fonts.push(font);
		}
	}
	(LazyHash::new(book), fonts)
});

/// Normalizes `path` the way typst resolves an `#import`, so a caller's key and a lookup
/// from inside a template agree.
pub(crate) fn key(path: &str) -> ClResult<String> {
	VirtualPath::new(path)
		.map(|v| v.get_without_slash().to_owned())
		.map_err(|e| Error::internal(format!("mintworks-pdf: bad template path {path:?}: {e:?}")))
}

pub(crate) struct DocWorld<'a> {
	library: LazyHash<Library>,
	main: FileId,
	files: &'a Files,
	/// Parsed once up front: typst asks for a source on every access, and `Source::new`
	/// re-parses.
	sources: HashMap<String, Source>,
}

impl<'a> DocWorld<'a> {
	/// Each `inputs` entry is readable as `sys.inputs.<key>`, as a string.
	pub(crate) fn new(
		files: &'a Files,
		main: &str,
		inputs: &BTreeMap<String, String>,
	) -> ClResult<Self> {
		let mut dict = Dict::new();
		for (k, v) in inputs {
			dict.insert(k.as_str().into(), Value::Str(v.as_str().into()));
		}

		let main_key = key(main)?;
		if !files.contains_key(&main_key) {
			return Err(Error::internal(format!(
				"mintworks-pdf: main file {main_key:?} not in the set"
			)));
		}
		let mut sources = HashMap::new();
		let mut main_id = None;
		for (path, bytes) in files
			.iter()
			.filter(|(p, _)| std::path::Path::new(p).extension() == Some("typ".as_ref()))
		{
			let id = file_id(path)?;
			let text = String::from_utf8(bytes.clone())
				.map_err(|_| Error::internal(format!("mintworks-pdf: {path:?} is not UTF-8")))?;
			if *path == main_key {
				main_id = Some(id);
			}
			sources.insert(path.clone(), Source::new(id, text));
		}
		let main = main_id.ok_or_else(|| {
			Error::internal(format!("mintworks-pdf: main file {main_key:?} is not .typ"))
		})?;

		Ok(Self {
			library: LazyHash::new(Library::builder().with_inputs(dict).build()),
			main,
			files,
			sources,
		})
	}
}

fn file_id(path: &str) -> ClResult<FileId> {
	let vpath = VirtualPath::new(path).map_err(|e| {
		Error::internal(format!("mintworks-pdf: bad template path {path:?}: {e:?}"))
	})?;
	Ok(RootedPath::new(VirtualRoot::Project, vpath).intern())
}

/// The set's key for `id`, or `NotFound` for anything under a package root.
fn lookup(id: FileId) -> FileResult<String> {
	let path = id.vpath().get_without_slash();
	match id.root() {
		VirtualRoot::Project => Ok(path.to_owned()),
		VirtualRoot::Package(_) => Err(FileError::NotFound(path.into())),
	}
}

impl typst::World for DocWorld<'_> {
	fn library(&self) -> &LazyHash<Library> {
		&self.library
	}

	fn book(&self) -> &LazyHash<FontBook> {
		&FONTS.0
	}

	fn main(&self) -> FileId {
		self.main
	}

	fn source(&self, id: FileId) -> FileResult<Source> {
		let path = lookup(id)?;
		self.sources.get(&path).cloned().ok_or_else(|| FileError::NotFound(path.into()))
	}

	fn file(&self, id: FileId) -> FileResult<Bytes> {
		let path = lookup(id)?;
		self.files
			.get(&path)
			.map(|b| Bytes::new(b.clone()))
			.ok_or_else(|| FileError::NotFound(path.into()))
	}

	fn font(&self, index: usize) -> Option<Font> {
		FONTS.1.get(index).cloned()
	}

	/// `None`: a PDF must not vary with the clock.
	fn today(&self, _offset: Option<Duration>) -> Option<Datetime> {
		None
	}
}

// vim: ts=4
