//! The skill registry: `skills/<name>/SKILL.md` (+ `SKILL.<lang>.md`, `references/<topic>.md`,
//! `references/<topic>.<lang>.md`) from an app directory, loaded and validated once at boot. A
//! read is a map lookup, so no path a model sends can reach the filesystem.

use std::{
	collections::BTreeMap,
	fmt,
	path::{Path, PathBuf},
};

use mintworks_core::{ClResult, Error};
use serde::Serialize;

/// The body's path, also what a read with no `path` serves.
pub const BODY: &str = "SKILL.md";
const MAX_DESCRIPTION: usize = 200;
const MAX_FILE: u64 = 64 * 1024;
const MAX_TOTAL: usize = 1024 * 1024;

#[derive(Clone, Debug)]
struct Skill {
	description: String,
	/// `(base path, lang) → text`; the base file is stored under `en`.
	files: BTreeMap<(String, String), String>,
}

/// Loaded once at boot; editing a skill needs a restart.
#[derive(Clone, Debug, Default)]
pub struct Skills(BTreeMap<String, Skill>);

/// A skill as a menu or `skills::list()` shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SkillEntry {
	pub name: String,
	pub description: String,
	/// Base paths, sorted.
	pub references: Vec<String>,
}

/// One served file: the `skill_read` result and the `skills::read()` value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SkillFile {
	pub name: String,
	pub path: String,
	/// The variant served; `en` for the base file.
	pub lang: String,
	/// `content.len()`.
	pub bytes: usize,
	/// Only on a body read.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub references: Option<Vec<String>>,
	pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadError {
	NoSkill(String),
	/// The skill, the path asked for, and every base path it has.
	NoFile {
		name: String,
		path: String,
		has: Vec<String>,
	},
}

impl fmt::Display for ReadError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::NoSkill(n) => write!(f, "no skill {n}"),
			Self::NoFile { name, path, has } => {
				write!(f, "skill {name} has no file {path}; it has: {has:?}")
			}
		}
	}
}

impl Skill {
	fn references(&self) -> Vec<String> {
		self.files
			.keys()
			.filter(|(p, l)| l == "en" && p != BODY)
			.map(|(p, _)| p.clone())
			.collect()
	}
}

impl Skills {
	/// Load every skill under `<app_dir>/skills/`. A missing `skills/` is an empty registry.
	///
	/// # Errors
	/// An unreadable file, or any file breaking the layout or frontmatter rules, naming its path.
	pub fn load(app_dir: &Path) -> ClResult<Self> {
		let root = app_dir.join("skills");
		let mut out = BTreeMap::new();
		let mut total = 0;
		match std::fs::symlink_metadata(&root) {
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self(out)),
			Err(e) => return Err(bad(&root, &e.to_string())),
			Ok(m) if m.is_symlink() => return Err(bad(&root, "a symlink is not allowed")),
			Ok(_) => {}
		}
		for dir in read_dir(&root)? {
			let Some(name) = file_name(&dir) else { continue };
			if !dir.is_dir() {
				return Err(bad(&dir, "not a skill directory"));
			}
			if !valid_name(name) {
				return Err(bad(&dir, "a skill name is [a-z0-9-]{1,64}"));
			}
			let skill = load_skill(&dir, name)?;
			total += skill.files.values().map(String::len).sum::<usize>();
			if total > MAX_TOTAL {
				return Err(bad(&root, "over 1 MiB in total"));
			}
			out.insert(name.to_owned(), skill);
		}
		Ok(Self(out))
	}

	/// Every skill, sorted by name.
	#[must_use]
	pub fn list(&self) -> Vec<SkillEntry> {
		self.0.keys().filter_map(|n| self.get(n)).collect()
	}

	#[must_use]
	pub fn get(&self, name: &str) -> Option<SkillEntry> {
		self.0.get(name).map(|s| SkillEntry {
			name: name.to_owned(),
			description: s.description.clone(),
			references: s.references(),
		})
	}

	/// `path` (a base path; `None` is the body) in `lang`, falling back to the base file.
	///
	/// # Errors
	/// An unknown skill, or a path that is not one of its base paths.
	pub fn read(&self, name: &str, path: Option<&str>, lang: &str) -> Result<SkillFile, ReadError> {
		let skill = self.0.get(name).ok_or_else(|| ReadError::NoSkill(name.to_owned()))?;
		let path = path.unwrap_or(BODY);
		let lang = mintworks_llm::prompts::lang_code(lang);
		let (served, content) = [lang.as_str(), "en"]
			.into_iter()
			.find_map(|l| skill.files.get(&(path.to_owned(), l.to_owned())).map(|c| (l, c)))
			.ok_or_else(|| ReadError::NoFile {
				name: name.to_owned(),
				path: path.to_owned(),
				has: std::iter::once(BODY.to_owned()).chain(skill.references()).collect(),
			})?;
		Ok(SkillFile {
			name: name.to_owned(),
			path: path.to_owned(),
			lang: served.to_owned(),
			bytes: content.len(),
			references: (path == BODY).then(|| skill.references()),
			content: content.clone(),
		})
	}
}

fn load_skill(dir: &Path, name: &str) -> ClResult<Skill> {
	let mut files = BTreeMap::new();
	let mut description = None;
	for path in read_dir(dir)? {
		let Some(file) = file_name(&path) else { continue };
		if file == "references" && path.is_dir() {
			for r in read_dir(&path)? {
				let Some(rf) = file_name(&r) else { continue };
				if r.is_dir() {
					return Err(bad(&r, "references/ holds files only"));
				}
				let (topic, lang) =
					split_variant(rf).ok_or_else(|| bad(&r, "not <topic>[.<lang>].md"))?;
				let text = if lang == "en" { read(&r)? } else { variant(&r, read(&r)?)? };
				files.insert((format!("references/{topic}.md"), lang.to_owned()), text);
			}
			continue;
		}
		match split_variant(file) {
			Some(("SKILL", lang)) if path.is_file() => {
				let text = read(&path)?;
				if lang == "en" {
					let (desc, body) = frontmatter(&path, &text, name)?;
					description = Some(desc);
					files.insert((BODY.to_owned(), "en".to_owned()), body);
				} else {
					files.insert((BODY.to_owned(), lang.to_owned()), variant(&path, text)?);
				}
			}
			_ => return Err(bad(&path, "not an allowed skill file")),
		}
	}
	let description = description.ok_or_else(|| bad(&dir.join(BODY), "missing"))?;
	// The `en` fallback must exist for every variant, or a read in another language would miss.
	if let Some((p, l)) = files
		.keys()
		.find(|(p, l)| l != "en" && !files.contains_key(&(p.clone(), "en".to_owned())))
	{
		let file = p.strip_suffix(".md").map_or_else(|| p.clone(), |s| format!("{s}.{l}.md"));
		return Err(bad(&dir.join(file), "a variant without its base file"));
	}
	Ok(Skill { description, files })
}

/// `x.md` → `(x, "en")`, `x.hu.md` → `(x, "hu")`; `x.en.md` (it would collide with `x.md`),
/// anything else, or a dotted topic, is `None`.
fn split_variant(file: &str) -> Option<(&str, &str)> {
	let stem = file.strip_suffix(".md")?;
	let (base, lang) = match stem.rsplit_once('.') {
		Some((_, "en")) => return None,
		Some((b, l)) if l.len() == 2 && l.bytes().all(|c| c.is_ascii_lowercase()) => (b, l),
		Some(_) => return None,
		None => (stem, "en"),
	};
	(!base.is_empty() && !base.contains('.')).then_some((base, lang))
}

/// Rejects frontmatter: a `---` line followed by a key [`frontmatter`] knows. A bare `---` is a
/// markdown rule, and `---` then `note: x` is prose.
fn variant(path: &Path, text: String) -> ClResult<String> {
	let mut lines = text.lines();
	if lines.next().is_some_and(|l| l.trim() == "---")
		&& lines
			.next()
			.and_then(|l| l.split_once(':'))
			.is_some_and(|(k, _)| matches!(k.trim(), "name" | "description" | "tools"))
	{
		return Err(bad(path, "a variant carries no frontmatter"));
	}
	Ok(text)
}

/// The `description` and the body after the frontmatter.
fn frontmatter(path: &Path, text: &str, dir_name: &str) -> ClResult<(String, String)> {
	let mut lines = text.split_inclusive('\n');
	let first = lines.next().unwrap_or_default();
	if first.trim_end() != "---" {
		return Err(bad(path, "must start with a --- frontmatter line"));
	}
	let (mut name, mut description) = (None, None);
	let mut closed = false;
	let mut consumed = first.len();
	for line in lines.by_ref() {
		consumed += line.len();
		let l = line.trim();
		if l == "---" {
			closed = true;
			break;
		}
		if l.is_empty() || l.starts_with('#') {
			continue;
		}
		if line.starts_with([' ', '\t']) {
			return Err(bad(path, &format!("nested frontmatter is not supported: {l}")));
		}
		let (k, v) = l
			.split_once(':')
			.ok_or_else(|| bad(path, &format!("not a key: value line: {l}")))?;
		let v = unquote(v.trim());
		if v.is_empty() || v == "|" || v == ">" {
			return Err(bad(path, &format!("{} needs a one-line value", k.trim())));
		}
		let slot = match k.trim() {
			"name" => &mut name,
			"description" => &mut description,
			"tools" => return Err(bad(path, "skill-scoped tools are not supported yet")),
			k => return Err(bad(path, &format!("unknown frontmatter key {k}"))),
		};
		if slot.replace(v.to_owned()).is_some() {
			return Err(bad(path, &format!("duplicate frontmatter key {}", k.trim())));
		}
	}
	if !closed {
		return Err(bad(path, "the frontmatter has no closing ---"));
	}
	let name = name.ok_or_else(|| bad(path, "frontmatter needs name"))?;
	if name != dir_name {
		return Err(bad(path, &format!("name {name} differs from its directory {dir_name}")));
	}
	let description = description.ok_or_else(|| bad(path, "frontmatter needs description"))?;
	if description.chars().count() > MAX_DESCRIPTION {
		return Err(bad(path, "description is over 200 characters"));
	}
	let body = text.get(consumed..).unwrap_or_default();
	Ok((description, body.trim_start_matches(['\r', '\n']).to_owned()))
}

fn unquote(v: &str) -> &str {
	for q in ['"', '\''] {
		if let Some(s) = v.strip_prefix(q).and_then(|s| s.strip_suffix(q)) {
			return s;
		}
	}
	v
}

fn valid_name(n: &str) -> bool {
	(1..=64).contains(&n.len())
		&& n.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// The entry's name, `None` for a dotfile (`.DS_Store`) or a non-UTF-8 name.
fn file_name(p: &Path) -> Option<&str> {
	p.file_name().and_then(|n| n.to_str()).filter(|n| !n.starts_with('.'))
}

/// The entries of `dir`; a symlink fails, so every later check sees only real files and dirs.
fn read_dir(dir: &Path) -> ClResult<Vec<PathBuf>> {
	let mut out = Vec::new();
	for e in std::fs::read_dir(dir).map_err(|e| bad(dir, &e.to_string()))? {
		let e = e.map_err(|e| bad(dir, &e.to_string()))?;
		let path = e.path();
		if e.file_type().map_err(|e| bad(&path, &e.to_string()))?.is_symlink() {
			return Err(bad(&path, "a symlink is not allowed"));
		}
		out.push(path);
	}
	out.sort();
	Ok(out)
}

fn read(path: &Path) -> ClResult<String> {
	let len = std::fs::metadata(path).map_err(|e| bad(path, &e.to_string()))?.len();
	if len > MAX_FILE {
		return Err(bad(path, "over 64 KiB"));
	}
	let s = std::fs::read_to_string(path).map_err(|e| bad(path, &e.to_string()))?;
	// An editor's BOM would otherwise fail the frontmatter's `---` check.
	Ok(s.strip_prefix('\u{feff}').map(str::to_owned).unwrap_or(s))
}

fn bad(path: &Path, msg: &str) -> Error {
	Error::internal(format!("skill {}: {msg}", path.display()))
}

#[cfg(test)]
mod tests {
	use super::*;

	const BASE: &str =
		"---\nname: demo\n# a comment\n\ndescription: \"Use when testing.\"\n---\n\nBody.\n";

	/// A fresh app dir per test, holding `files` under `skills/`.
	fn app(tag: &str, files: &[(&str, &str)]) -> PathBuf {
		let root = std::env::temp_dir()
			.join(format!("mintworks-agent-skills-{tag}-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&root);
		for (p, text) in files {
			let p = root.join("skills").join(p);
			std::fs::create_dir_all(p.parent().unwrap()).unwrap();
			std::fs::write(p, text).unwrap();
		}
		std::fs::create_dir_all(&root).unwrap();
		root
	}

	fn load(tag: &str, files: &[(&str, &str)]) -> ClResult<Skills> {
		let root = app(tag, files);
		let r = Skills::load(&root);
		std::fs::remove_dir_all(&root).unwrap();
		r
	}

	fn fails(tag: &str, files: &[(&str, &str)], needle: &str) {
		let e = load(tag, files).unwrap_err().to_string();
		assert!(e.contains(needle), "{tag}: {e}");
	}

	#[test]
	fn loads_and_falls_back_to_the_base_file() {
		let s = load(
			"ok",
			&[
				("demo/SKILL.md", BASE),
				("demo/SKILL.hu.md", "Törzs.\n"),
				("demo/references/check.md", "Check.\n"),
				("demo/references/more.md", "More.\n"),
				("demo/references/more.hu.md", "Több.\n"),
				("demo/.DS_Store", "x"),
				(".hidden", "x"),
			],
		)
		.unwrap();
		let refs = vec!["references/check.md".to_owned(), "references/more.md".to_owned()];
		assert_eq!(
			s.list(),
			vec![SkillEntry {
				name: "demo".into(),
				description: "Use when testing.".into(),
				references: refs.clone()
			}]
		);
		let body = s.read("demo", None, "en").unwrap();
		assert_eq!((body.content.as_str(), body.lang.as_str()), ("Body.\n", "en"));
		assert_eq!((body.path.as_str(), body.bytes), ("SKILL.md", 6));
		assert_eq!(body.references, Some(refs));
		let hu = s.read("demo", Some("SKILL.md"), "hu").unwrap();
		assert_eq!((hu.content.as_str(), hu.lang.as_str()), ("Törzs.\n", "hu"));
		let check = s.read("demo", Some("references/check.md"), "hu").unwrap();
		assert_eq!((check.content.as_str(), check.lang.as_str()), ("Check.\n", "en"));
		assert_eq!(check.references, None);
		assert_eq!(s.read("demo", Some("references/more.md"), "hu").unwrap().lang, "hu");
		assert_eq!(s.read("demo", None, "hu-HU").unwrap().lang, "hu");
		assert_eq!(s.read("demo", None, "HU").unwrap().lang, "hu");
	}

	#[test]
	fn a_bom_is_stripped() {
		let s = load("bom", &[("demo/SKILL.md", &format!("\u{feff}{BASE}"))]).unwrap();
		assert_eq!(s.read("demo", None, "en").unwrap().content, "Body.\n");
	}

	#[test]
	fn over_a_mib_in_total_fails_the_boot() {
		let big = "x".repeat(63 * 1024);
		let topics: Vec<String> = (0..17).map(|i| format!("demo/references/t{i}.md")).collect();
		let mut files = vec![("demo/SKILL.md", BASE)];
		files.extend(topics.iter().map(|p| (p.as_str(), big.as_str())));
		fails("total", &files, "over 1 MiB in total");
	}

	#[test]
	fn a_missing_file_or_skill_is_a_read_error() {
		let s = load("miss", &[("demo/SKILL.md", BASE)]).unwrap();
		assert_eq!(s.read("nope", None, "en"), Err(ReadError::NoSkill("nope".into())));
		for p in ["references/x.md", "../demo/SKILL.md", "SKILL.hu.md", "/etc/passwd"] {
			let e = s.read("demo", Some(p), "en").unwrap_err();
			assert_eq!(
				e.to_string(),
				format!("skill demo has no file {p}; it has: [\"SKILL.md\"]")
			);
		}
	}

	#[test]
	fn no_skills_dir_is_empty() {
		assert!(load("none", &[]).unwrap().list().is_empty());
	}

	#[test]
	fn a_variant_may_open_with_a_horizontal_rule() {
		let s = load(
			"rule",
			&[
				("demo/SKILL.md", BASE),
				("demo/SKILL.hu.md", "---\n\nText"),
				("demo/references/a.md", "A"),
				("demo/references/a.hu.md", "---\n\nText"),
			],
		)
		.unwrap();
		assert_eq!(s.read("demo", None, "hu").unwrap().content, "---\n\nText");
	}

	#[test]
	fn only_a_known_key_after_the_rule_is_variant_frontmatter() {
		let s = load("note", &[("demo/SKILL.md", BASE), ("demo/SKILL.hu.md", "---\nnote: x\n")])
			.unwrap();
		assert_eq!(s.read("demo", None, "hu").unwrap().content, "---\nnote: x\n");
		fails(
			"desc",
			&[("demo/SKILL.md", BASE), ("demo/SKILL.hu.md", "---\ndescription: x\n")],
			"a variant carries no frontmatter",
		);
	}

	#[cfg(unix)]
	#[test]
	fn a_symlink_fails_the_boot() {
		use std::os::unix::fs::symlink;
		let root = app("link", &[("demo/SKILL.md", BASE), ("real/SKILL.md", BASE)]);
		let skills = root.join("skills");
		std::fs::create_dir(skills.join("demo/references")).unwrap();
		symlink(skills.join("demo/SKILL.md"), skills.join("demo/references/a.md")).unwrap();
		let e = Skills::load(&root).unwrap_err().to_string();
		assert!(e.contains("a.md: a symlink is not allowed"), "{e}");
		std::fs::remove_dir_all(skills.join("demo")).unwrap();
		symlink(skills.join("real"), skills.join("linked")).unwrap();
		let e = Skills::load(&root).unwrap_err().to_string();
		assert!(e.contains("linked: a symlink is not allowed"), "{e}");
		std::fs::remove_dir_all(&root).unwrap();
	}

	type Files<'a> = Vec<(&'a str, String)>;

	#[test]
	fn every_layout_and_frontmatter_rule_fails_the_boot() {
		let fm = |body: &str| format!("---\n{body}\n---\nBody.\n");
		let cases: &[(&str, Files, &str)] = &[
			("nobody", vec![("demo/references/a.md", "A".into())], "SKILL.md: missing"),
			("nofm", vec![("demo/SKILL.md", "Body.\n".into())], "must start with"),
			("unclosed", vec![("demo/SKILL.md", "---\nname: demo\n".into())], "no closing"),
			(
				"unknown",
				vec![("demo/SKILL.md", fm("name: demo\ndescription: d\nversion: 1"))],
				"unknown frontmatter key version",
			),
			(
				"tools",
				vec![("demo/SKILL.md", fm("name: demo\ndescription: d\ntools: a"))],
				"skill-scoped tools are not supported yet",
			),
			("noname", vec![("demo/SKILL.md", fm("description: d"))], "needs name"),
			("nodesc", vec![("demo/SKILL.md", fm("name: demo"))], "needs description"),
			(
				"mismatch",
				vec![("demo/SKILL.md", fm("name: other\ndescription: d"))],
				"differs from its directory",
			),
			("badname", vec![("Demo/SKILL.md", fm("name: Demo\ndescription: d"))], "[a-z0-9-]"),
			(
				"long",
				vec![(
					"demo/SKILL.md",
					fm(&format!("name: demo\ndescription: {}", "x".repeat(201))),
				)],
				"over 200",
			),
			(
				"nested",
				vec![("demo/SKILL.md", fm("name: demo\ndescription: d\nmeta:\n  a: b"))],
				"needs a one-line value",
			),
			(
				"indented",
				vec![("demo/SKILL.md", fm("name: demo\n  description: d"))],
				"nested frontmatter",
			),
			(
				"multiline",
				vec![("demo/SKILL.md", fm("name: demo\ndescription: >\n  d"))],
				"one-line value",
			),
			(
				"stray",
				vec![("demo/SKILL.md", BASE.into()), ("demo/notes.txt", "x".into())],
				"notes.txt: not an allowed skill file",
			),
			(
				"subdir",
				vec![("demo/SKILL.md", BASE.into()), ("demo/assets/a.md", "x".into())],
				"assets: not an allowed skill file",
			),
			(
				"deep",
				vec![("demo/SKILL.md", BASE.into()), ("demo/references/x/a.md", "x".into())],
				"files only",
			),
			("topfile", vec![("README.md", "x".into())], "not a skill directory"),
			(
				"orphan",
				vec![("demo/SKILL.md", BASE.into()), ("demo/references/a.hu.md", "x".into())],
				"a.hu.md: a variant without its base file",
			),
			(
				"varfm",
				vec![("demo/SKILL.md", BASE.into()), ("demo/SKILL.hu.md", fm("name: demo"))],
				"SKILL.hu.md: a variant carries no frontmatter",
			),
			(
				"expliciten",
				vec![("demo/SKILL.md", BASE.into()), ("demo/SKILL.en.md", "x".into())],
				"not an allowed skill file",
			),
			(
				"huge",
				vec![
					("demo/SKILL.md", BASE.into()),
					("demo/references/a.md", "x".repeat(65 * 1024)),
				],
				"a.md: over 64 KiB",
			),
			(
				"badlang",
				vec![("demo/SKILL.md", BASE.into()), ("demo/SKILL.hun.md", "x".into())],
				"not an allowed skill file",
			),
		];
		for (tag, files, needle) in cases {
			let files: Vec<(&str, &str)> = files.iter().map(|(p, t)| (*p, t.as_str())).collect();
			fails(tag, &files, needle);
		}
	}
}

// vim: ts=4
