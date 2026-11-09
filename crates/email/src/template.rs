// SPDX-License-Identifier: MPL-2.0
//! Handlebars rendering.
//!
//! A template is a pair of files — `<name>.html.hbs` and `<name>.txt.hbs` — each opening
//! with YAML frontmatter carrying `layout:` and `subject:`. The layout of the same
//! extension wraps the rendered body, receiving it as `body` and the rendered subject as
//! `title`, and the request's primary language subtag as `lang`.
//!
//! Each dir's `partials/<name>.{html,txt}.hbs` is a partial of that format, `{{> name}}`; an
//! app dir's partial replaces the framework's of the same name.
//!
//! Rendering is strict: a variable the template names and the caller did not supply is an
//! error, not an empty string. A mail with a blank activation link is worse than a job
//! that fails loudly and retries.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use handlebars::Handlebars;
use mintworks_core::{ClResult, Error, error::StatusCode};
use parking_lot::{Mutex, RwLock};
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

/// The two registries of one template dir list: `Handlebars::new` registers every built-in
/// helper, and `register_template_string` keeps each template parsed under the [`SOURCES`] key.
struct Registries {
	html: RwLock<Handlebars<'static>>,
	/// The text part and the subject are not HTML: escaping there corrupts links and `&`.
	plain: RwLock<Handlebars<'static>>,
}

/// Per dir list, because partials resolve by bare name: one registry shared by two lists
/// would hand one the other's override. A process has one list (app, framework).
static REGISTRIES: LazyLock<Mutex<HashMap<Vec<PathBuf>, Arc<Registries>>>> =
	LazyLock::new(|| Mutex::new(HashMap::new()));

fn strict() -> Handlebars<'static> {
	let mut hb = Handlebars::new();
	hb.set_strict_mode(true);
	hb
}

/// `dirs`' registries, built with their partials the first time the list is seen.
fn registries(dirs: &[PathBuf]) -> ClResult<Arc<Registries>> {
	if let Some(hit) = REGISTRIES.lock().get(dirs) {
		return Ok(Arc::clone(hit));
	}
	let (mut html, mut plain) = (strict(), strict());
	plain.register_escape_fn(handlebars::no_escape);
	// Framework first, so an earlier (app) dir's partial replaces it.
	for dir in dirs.iter().rev() {
		let Ok(entries) = std::fs::read_dir(dir.join("partials")) else { continue };
		for entry in entries.flatten() {
			let path = entry.path();
			let file = path.file_name().and_then(|f| f.to_str()).unwrap_or_default();
			let (name, hb) = if let Some(n) = file.strip_suffix(".html.hbs") {
				(n, &mut html)
			} else if let Some(n) = file.strip_suffix(".txt.hbs") {
				(n, &mut plain)
			} else {
				continue;
			};
			let src = std::fs::read_to_string(&path)
				.map_err(|e| template_error(format!("email partial {}: {e}", path.display())))?;
			hb.register_partial(name, src)
				.map_err(|e| template_error(format!("email partial {}: {e}", path.display())))?;
		}
	}
	let regs = Arc::new(Registries { html: RwLock::new(html), plain: RwLock::new(plain) });
	REGISTRIES.lock().insert(dirs.to_vec(), Arc::clone(&regs));
	Ok(regs)
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
#[cfg(test)]
fn load(dir: &std::path::Path, name: &str, lang: &str, ext: &str) -> ClResult<(String, String)> {
	load_in(&[dir.to_path_buf()], name, lang, ext)
}

/// The first dir holding `<name>.<lang>.<ext>.hbs` or `<name>.<ext>.hbs`, and that file; else
/// the last dir's plain file, whose read then fails.
fn find<'d>(
	dirs: &'d [PathBuf],
	name: &str,
	lang: &str,
	ext: &str,
) -> ClResult<(&'d PathBuf, PathBuf)> {
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
	let localized = format!("{name}.{lang}.{ext}.hbs");
	let plain = format!("{name}.{ext}.hbs");
	let files = if safe { vec![localized, plain.clone()] } else { vec![plain.clone()] };
	dirs.iter()
		.find_map(|d| files.iter().map(|f| d.join(f)).find(|p| p.is_file()).map(|p| (d, p)))
		.or_else(|| dirs.last().map(|d| (d, d.join(&plain))))
		.ok_or_else(|| Error::internal("no email template directory configured"))
}

/// [`load`] over several directories, earliest first: the app dir overrides the framework per
/// file, and a dir lacking the name defers to the next.
fn load_in(dirs: &[PathBuf], name: &str, lang: &str, ext: &str) -> ClResult<(String, String)> {
	let (_, path) = find(dirs, name, lang, ext)?;
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
#[cfg(test)]
pub fn render(dir: &std::path::Path, name: &str, lang: &str, vars: &Value) -> ClResult<Rendered> {
	render_in(&[dir.to_path_buf()], name, lang, vars)
}

/// [`render`] from the first of `dirs` holding the html part — the app's before the framework's.
/// Both parts come from that one dir, so a mail never pairs an app's html with another's text.
pub fn render_in(dirs: &[PathBuf], name: &str, lang: &str, vars: &Value) -> ClResult<Rendered> {
	let (dir, _) = find(dirs, name, lang, "html")?;
	let own = std::slice::from_ref(dir);
	let regs = registries(dirs)?;
	let mut subject = String::new();
	let mut out = [String::new(), String::new()];
	for (slot, ext) in out.iter_mut().zip(["html", "txt"]) {
		let hb = if ext == "html" { &regs.html } else { &regs.plain };
		let (key, src) = load_in(own, name, lang, ext)?;
		let (meta, body_src) = split(&src);
		let body =
			render_cached(hb, &key, body_src, vars, &format!("email template {name}.{ext}"))?;
		if subject.is_empty()
			&& let Some(s) = &meta.subject
		{
			let skey = format!("{key}#subject");
			subject =
				render_cached(&regs.plain, &skey, s, vars, &format!("email subject {name}.{ext}"))?;
		}
		*slot = match &meta.layout {
			None => body,
			Some(layout) => {
				let layouts: Vec<PathBuf> = dirs.iter().map(|d| d.join("layouts")).collect();
				// One dir for both parts, as for the body: an app overriding only the html layout
				// must fail, not pair it with the framework's text layout.
				let (ldir, _) = find(&layouts, layout, lang, "html")?;
				let (lkey, src) = load_in(std::slice::from_ref(ldir), layout, lang, ext)?;
				let mut lvars = match vars {
					Value::Object(m) => m.clone(),
					_ => Map::new(),
				};
				lvars.insert("body".to_owned(), Value::String(body));
				lvars.insert("title".to_owned(), Value::String(subject.clone()));
				// `hu-en` is no language tag; its document opens in Hungarian.
				let primary = lang.split('-').next().unwrap_or(lang);
				lvars.insert("lang".to_owned(), Value::String(primary.to_owned()));
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
	use std::path::Path;

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

	/// `mintworks_invoice::mail`'s two templates in each language its `doc_lang` yields, with the
	/// vars that producer passes.
	#[test]
	fn the_invoice_mails_render_in_every_document_language() {
		let vars = json!({
			"tenantName": "Teszt Kft.", "number": "A2026/000001", "amount": "127 000 Ft",
			"dueDate": "2026. 11. 30.", "periodStart": "", "periodEnd": "", "intro": "Szia",
			"subjectPrefix": "[TESZT] ", "downloadUrl": "https://x.test/d/abc",
			"sha256": "f00d", "originalNumber": "A2026/000000",
		});
		for name in ["invoice_mail", "storno_mail"] {
			for lang in ["hu", "en", "hu-en"] {
				let r = render(dir(), name, lang, &vars).unwrap();
				assert!(
					r.subject.starts_with("[TESZT] Teszt Kft."),
					"{name}.{lang}: {}",
					r.subject
				);
				assert!(r.subject.contains("A2026/000001"), "{name}.{lang}");
				assert!(r.html.starts_with("<!DOCTYPE"), "{name}.{lang}: no layout");
				for part in [&r.html, &r.text] {
					assert!(
						part.contains("https://x.test/d/abc") && part.contains("f00d"),
						"{name}.{lang}"
					);
					assert!(part.contains("Szia"), "{name}.{lang}: no intro");
				}
				if name == "storno_mail" {
					assert!(r.text.contains("A2026/000000"), "{name}.{lang}: no original number");
				}
			}
		}
	}

	/// A fresh app template dir holding `files`; `SOURCES` is keyed by path, so one per test.
	fn app_dir(test: &str, files: &[(&str, &str)]) -> PathBuf {
		let d = std::env::temp_dir().join(format!("mintworks-email-{}-{test}", std::process::id()));
		let _ = std::fs::remove_dir_all(&d);
		std::fs::create_dir_all(&d).unwrap();
		for (f, src) in files {
			std::fs::write(d.join(f), src).unwrap();
		}
		d
	}

	#[test]
	fn an_app_plain_template_beats_the_framework_localized_one() {
		let app = app_dir("plain", &[("activation.html.hbs", "app")]);
		let dirs = [app, dir().to_path_buf()];
		assert_eq!(load_in(&dirs, "activation", "hu", "html").unwrap().1, "app");
	}

	#[test]
	fn an_app_dir_lacking_the_template_defers_to_the_framework() {
		let app = app_dir("lacking", &[("other.html.hbs", "app")]);
		let dirs = [app, dir().to_path_buf()];
		let hu = load(dir(), "activation", "hu", "html").unwrap().1;
		assert_eq!(load_in(&dirs, "activation", "hu", "html").unwrap().1, hu);
	}

	#[test]
	fn one_mail_never_mixes_an_app_html_with_the_framework_text() {
		let app = app_dir("html-only", &[("activation.html.hbs", "app html")]);
		let dirs = [app, dir().to_path_buf()];
		assert!(render_in(&dirs, "activation", "en", &json!({})).is_err());
	}

	#[test]
	fn one_mail_never_mixes_an_app_html_layout_with_the_framework_text_layout() {
		let page = "---\nlayout: default\nsubject: s\n---\nhi";
		let app = app_dir("layout-html-only", &[("mixed.html.hbs", page), ("mixed.txt.hbs", page)]);
		std::fs::create_dir_all(app.join("layouts")).unwrap();
		std::fs::write(app.join("layouts/default.html.hbs"), "{{{body}}}").unwrap();
		let dirs = [app, dir().to_path_buf()];
		assert!(render_in(&dirs, "mixed", "en", &json!({})).is_err());
	}

	#[test]
	fn a_partial_resolves_and_an_app_partial_replaces_the_framework_one() {
		let page = "---\nsubject: s\n---\n{{> invoice_mail_en}}";
		let vars = json!({
			"number": "N1", "amount": "1 Ft", "dueDate": "", "periodStart": "", "periodEnd": "",
			"downloadUrl": "https://x.test/d", "sha256": "ab", "tenantName": "T",
		});
		let app = app_dir("partial", &[("p.html.hbs", page), ("p.txt.hbs", page)]);
		let both = [app.clone(), dir().to_path_buf()];
		let r = render_in(&both, "p", "en", &vars).unwrap();
		assert!(
			r.html.contains("Download the invoice (PDF)") && r.html.contains("N1"),
			"{}",
			r.html
		);
		assert!(r.text.contains("Download the invoice: https://x.test/d"), "{}", r.text);

		std::fs::create_dir_all(app.join("partials")).unwrap();
		std::fs::write(app.join("partials/invoice_mail_en.html.hbs"), "mine {{number}}").unwrap();
		// A dir list seen before keeps its registry; a new list picks the override up.
		let other = [app, PathBuf::from("/nonexistent"), dir().to_path_buf()];
		let r = render_in(&other, "p", "en", &vars).unwrap();
		assert_eq!(r.html, "mine N1");
		assert!(r.text.contains("Download the invoice:"), "the txt partial is the framework's");
	}

	#[test]
	fn strict_mode_rejects_a_missing_variable() {
		assert!(render(dir(), "activation", "en", &json!({"name": "Anna"})).is_err());
	}
}

// vim: ts=4
