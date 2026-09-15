//! Handlebars rendering.
//!
//! A template is a pair of files — `<name>.html.hbs` and `<name>.txt.hbs` — each opening
//! with YAML frontmatter carrying `layout:` and `subject:`. The layout of the same
//! extension wraps the rendered body, receiving it as `body` and the rendered subject as
//! `title`.
//!
//! Rendering is strict: a variable the template names and the caller did not supply is an
//! error, not an empty string. A mail with a blank activation link is worse than a job
//! that fails loudly and retries.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use handlebars::Handlebars;
use parking_lot::{Mutex, RwLock};
use saas_core::{ClResult, Error, error::StatusCode};
use serde_json::{Map, Value};

/// Both formats of one rendered mail.
#[derive(Clone, Debug)]
pub struct Rendered {
	pub subject: String,
	pub html: String,
	pub text: String,
}

#[derive(Default)]
struct Meta {
	layout: Option<String>,
	/// Itself a handlebars template, so a subject can carry variables.
	subject: Option<String>,
}

/// Splits a leading `---\n…\n---` block off the body. No frontmatter leaves the defaults and
/// treats the whole file as body.
///
/// The block is two known keys, each a plain scalar on one line, so this splits on the first
/// `:` rather than pulling in a YAML parser. A surrounding pair of quotes is stripped (a
/// subject containing `: ` must be written quoted); anything else in the block is ignored.
fn split(src: &str) -> (Meta, &str) {
	let Some(rest) = src.strip_prefix("---\n") else { return (Meta::default(), src) };
	let Some(end) = rest.find("\n---") else { return (Meta::default(), src) };
	let body = rest[end + 4..].trim_start_matches('\n');

	let mut meta = Meta::default();
	for line in rest[..end].lines() {
		let Some((key, value)) = line.split_once(':') else { continue };
		let value = value.trim();
		let value = value
			.strip_prefix('"')
			.and_then(|v| v.strip_suffix('"'))
			.or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
			.unwrap_or(value);
		match key.trim() {
			"layout" => meta.layout = Some(value.to_owned()),
			"subject" => meta.subject = Some(value.to_owned()),
			_ => {}
		}
	}
	(meta, body)
}

/// Template sources by *resolved path*, which bounds this map — and the two registries keyed
/// alongside it — by the files on disk. Keyed on the request `(dir, name, lang, ext)` it cost
/// one `is_file` less per render, but every distinct `accounts.locale` that fell back to the
/// base file registered another copy of it, without limit.
/// `email.template_dir` is read once per path: editing a template needs a restart.
static SOURCES: LazyLock<Mutex<HashMap<String, String>>> =
	LazyLock::new(|| Mutex::new(HashMap::new()));

/// The two registries, built once: `Handlebars::new` registers every built-in helper, and
/// `register_template_string` keeps each template parsed under the [`SOURCES`] key.
static HTML: LazyLock<RwLock<Handlebars<'static>>> = LazyLock::new(|| RwLock::new(strict()));
/// The text part and the subject are not HTML: escaping there corrupts links and `&`.
static PLAIN: LazyLock<RwLock<Handlebars<'static>>> = LazyLock::new(|| {
	let mut hb = strict();
	hb.register_escape_fn(handlebars::no_escape);
	RwLock::new(hb)
});

fn strict() -> Handlebars<'static> {
	let mut hb = Handlebars::new();
	hb.set_strict_mode(true);
	hb
}

/// Render `src` under `key`, parsing it only the first time that key is seen.
fn render_cached(
	hb: &RwLock<Handlebars<'static>>,
	key: &str,
	src: &str,
	vars: &Value,
	what: &str,
) -> ClResult<String> {
	if !hb.read().has_template(key) {
		hb.write()
			.register_template_string(key, src)
			.map_err(|e| template_error(format!("{what}: {e}")))?;
	}
	hb.read().render(key, vars).map_err(|e| template_error(format!("{what}: {e}")))
}

/// A template that cannot be read or rendered, as a **retryable** error.
///
/// Never `Error::internal`, which is `Retry::Never`: nothing in the workspace re-drives a
/// `SEND_EMAIL` row, so terminating on a broken template loses the activation link it carried.
/// [`SOURCES`] caches sources for the life of the process, so the retry recovers only a
/// transient read failure — a corrected template still needs a restart, which the message says.
fn template_error(msg: impl Into<String>) -> Error {
	Error::coded_retry(
		StatusCode::SERVICE_UNAVAILABLE,
		"E-EMAIL-TEMPLATE",
		format!(
			"{}; a corrected template needs a restart — sources are cached process-wide",
			msg.into()
		),
	)
}

/// `<dir>/<name>.<lang>.<ext>.hbs`, falling back to `<dir>/<name>.<ext>.hbs`. Returns the
/// [`SOURCES`] key beside the source, which is also the name it is registered under.
fn load(dir: &Path, name: &str, lang: &str, ext: &str) -> ClResult<(String, String)> {
	// `name` reaches here from `jobs.payload`, and anything but a flat name escapes `dir` through
	// `Path::join`. Rejected rather than fallen back on — unlike `lang` there is nothing to fall
	// back *to* — and terminal, because a bad name is a producer bug, not misconfiguration.
	if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
	{
		return Err(Error::internal(format!("email template name `{name}` is not a bare name")));
	}
	// `lang` reaches here from `jobs.payload`. A component containing a separator or a dot
	// would escape `dir` through `Path::join`, so anything but a bare tag falls back.
	let safe = !lang.is_empty() && lang.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
	let localized = dir.join(format!("{name}.{lang}.{ext}.hbs"));
	let path =
		if safe && localized.is_file() { localized } else { dir.join(format!("{name}.{ext}.hbs")) };

	let key = path.display().to_string();
	if let Some(hit) = SOURCES.lock().get(&key) {
		return Ok((key, hit.clone()));
	}

	let src = std::fs::read_to_string(&path)
		.map_err(|e| template_error(format!("email template {}: {e}", path.display())))?;
	SOURCES.lock().insert(key.clone(), src.clone());
	Ok((key, src))
}

/// Renders `name` in `lang` into subject, html and text.
pub fn render(dir: &Path, name: &str, lang: &str, vars: &Value) -> ClResult<Rendered> {
	let mut subject = String::new();
	let mut out = [String::new(), String::new()];
	for (slot, ext) in out.iter_mut().zip(["html", "txt"]) {
		let hb = if ext == "html" { &*HTML } else { &*PLAIN };
		let (key, src) = load(dir, name, lang, ext)?;
		let (meta, body_src) = split(&src);
		let body =
			render_cached(hb, &key, body_src, vars, &format!("email template {name}.{ext}"))?;
		if subject.is_empty()
			&& let Some(s) = &meta.subject
		{
			let skey = format!("{key}#subject");
			subject =
				render_cached(&PLAIN, &skey, s, vars, &format!("email subject {name}.{ext}"))?;
		}
		*slot = match &meta.layout {
			None => body,
			Some(layout) => {
				let (lkey, src) = load(&dir.join("layouts"), layout, lang, ext)?;
				let mut lvars = match vars {
					Value::Object(m) => m.clone(),
					_ => Map::new(),
				};
				lvars.insert("body".to_owned(), Value::String(body));
				lvars.insert("title".to_owned(), Value::String(subject.clone()));
				render_cached(
					hb,
					&lkey,
					&src,
					&Value::Object(lvars),
					&format!("email layout {layout}.{ext}"),
				)?
			}
		};
	}
	let [html, text] = out;
	Ok(Rendered { subject, html, text })
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	/// The shipped templates, so this also fails when one of them names a variable its
	/// producer does not pass.
	fn dir() -> &'static Path {
		Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/email"))
	}

	#[test]
	fn frontmatter_splits_and_survives_garbage() {
		let (meta, body) = split("---\nlayout: default\nsubject: \"Hi {{name}}\"\n---\n<p>x</p>");
		assert_eq!(meta.layout.as_deref(), Some("default"));
		assert_eq!(meta.subject.as_deref(), Some("Hi {{name}}"));
		assert_eq!(body, "<p>x</p>");
		assert!(split("<p>no frontmatter</p>").0.subject.is_none());
		assert_eq!(split("---\n: : :\n---\nbody").1, "body");
	}

	/// Defence in depth: `lang` reaches `load` from `jobs.payload` and `Path::join` resolves
	/// `..`, so a traversing tag must fall back to the base template, never read outside `dir`.
	#[test]
	fn a_traversing_lang_falls_back_instead_of_escaping_the_dir() {
		let base = load(dir(), "activation", "en", "html").unwrap().1;
		for lang in ["../../x", "hu/../hu", "..", "hu.", ""] {
			assert_eq!(load(dir(), "activation", lang, "html").unwrap().1, base, "{lang}");
		}
		// A real tag with its own file still wins.
		assert_ne!(load(dir(), "activation", "hu", "html").unwrap().1, base);
	}

	/// Keyed on the request tuple, an unrecognised tag fell back to the base file but still
	/// registered its own copy of it — one permanent entry per distinct `accounts.locale`,
	/// of which `is_locale` admits millions.
	#[test]
	fn unknown_locales_share_one_cache_entry() {
		let junk = ["zz", "qqq", "xy", "ab-CD"];
		let base = load(dir(), "activation", "en", "html").unwrap().0;
		for lang in junk {
			assert_eq!(load(dir(), "activation", lang, "html").unwrap().0, base, "{lang}");
		}
		let leaked = SOURCES.lock().keys().filter(|k| junk.iter().any(|l| k.contains(l))).count();
		assert_eq!(leaked, 0, "an unrecognised locale must not key an entry of its own");
	}

	/// `name` comes from the same `jobs.payload` as `lang`, but there is no base template to
	/// fall back to, so a non-flat name is refused outright.
	#[test]
	fn a_traversing_template_name_is_refused() {
		for name in ["../../x", "a/b", "..", "activation.hu", ""] {
			assert!(load(dir(), name, "en", "html").is_err(), "{name}");
		}
		assert!(load(dir(), "activation", "en", "html").is_ok());
	}

	#[test]
	fn renders_both_locales_through_the_layout() {
		let vars = json!({"name": "Anna", "activation_link": "https://x.test/a?t=1"});
		let en = render(dir(), "activation", "en", &vars).unwrap();
		let hu = render(dir(), "activation", "hu", &vars).unwrap();

		// `en` has no infixed file: it must fall back to the base template, not fail.
		assert!(en.html.starts_with("<!DOCTYPE"), "layout was not applied");
		assert!(en.html.contains("Anna"), "{}", en.html);
		// Handlebars html-escapes `=` as `&#x3D;`; the browser decodes it back inside the href.
		assert!(en.html.contains("https://x.test/a?t&#x3D;1"), "{}", en.html);
		assert!(en.text.contains("https://x.test/a?t=1"));
		assert!(!en.subject.is_empty());
		// The subject is injected into the layout as `title`.
		assert!(en.html.contains(&en.subject));
		// `.hu.` files exist, so the two locales differ.
		assert_ne!(en.subject, hu.subject);
		assert_ne!(en.text, hu.text);
	}

	#[test]
	fn strict_mode_rejects_a_missing_variable() {
		assert!(render(dir(), "activation", "en", &json!({"name": "Anna"})).is_err());
	}
}

// vim: ts=4
