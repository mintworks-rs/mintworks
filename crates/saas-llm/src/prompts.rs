//! The prompt registry: handlebars files `packs/<pack>/prompts/**/<step>.<lang>.md` from an app
//! directory, keyed `<pack>/<step>.<lang>` (`<step>` keeps its sub-directory, `/`-joined).

use std::path::Path;

use handlebars::Handlebars;
use saas_core::{ClResult, Error};
use serde_json::Value;

use crate::service::config;

/// Loaded once at boot; editing a prompt needs a restart.
#[derive(Clone, Debug)]
pub struct Prompts {
	hb: Handlebars<'static>,
}

impl Prompts {
	/// Load every prompt under `<app_dir>/packs/*/prompts/`. A missing `packs/` is an empty
	/// registry, not an error.
	///
	/// # Errors
	/// An unreadable file or a template that does not parse.
	pub fn load(app_dir: &Path) -> ClResult<Self> {
		let mut hb = Handlebars::new();
		hb.set_strict_mode(true);
		// Prompts are not HTML: escaping would turn `&` and quotes into entities.
		hb.register_escape_fn(handlebars::no_escape);
		let packs = app_dir.join("packs");
		if packs.is_dir() {
			for pack in read_dir(&packs)? {
				let dir = pack.join("prompts");
				if let Some(name) = pack.file_name().and_then(|n| n.to_str())
					&& dir.is_dir()
				{
					load_dir(&mut hb, &dir, name)?;
				}
			}
		}
		Ok(Self { hb })
	}

	/// Render `<pack>/<step>` in `lang`, falling back to `en`, with `lang` added to `vars` and
	/// the language-pin line appended: every prompt fixes its output language.
	///
	/// # Errors
	/// `E-LLM-CONFIG` for an unknown prompt, non-object `vars`, or a strict-mode render error.
	pub fn render(&self, pack_step: &str, lang: &str, vars: &Value) -> ClResult<String> {
		let code = lang_code(lang);
		let key = [format!("{pack_step}.{code}"), format!("{pack_step}.en")]
			.into_iter()
			.find(|k| self.hb.has_template(k))
			.ok_or_else(|| config(format!("no prompt {pack_step} for {code} or en")))?;
		let mut vars = match vars {
			Value::Object(m) => m.clone(),
			Value::Null => serde_json::Map::new(),
			_ => return Err(config("prompt vars must be an object")),
		};
		vars.entry("lang").or_insert_with(|| Value::String(lang.to_owned()));
		let body = self
			.hb
			.render(&key, &Value::Object(vars))
			.map_err(|e| config(format!("prompt {key}: {e}")))?;
		Ok(format!(
			"{}\n\nWrite your answer in the language with ISO 639-1 code `{lang}`.",
			body.trim_end()
		))
	}
}

/// The bare lowercase ISO 639-1 code template and skill variants are named by: `hu-HU`, `hu_HU`
/// and `HU` are all `hu`.
#[must_use]
pub fn lang_code(lang: &str) -> String {
	lang.split(['-', '_']).next().unwrap_or_default().to_ascii_lowercase()
}

fn read_dir(dir: &Path) -> ClResult<Vec<std::path::PathBuf>> {
	let mut out = Vec::new();
	for e in std::fs::read_dir(dir).map_err(|e| io(dir, &e))? {
		out.push(e.map_err(|e| io(dir, &e))?.path());
	}
	out.sort();
	Ok(out)
}

fn load_dir(hb: &mut Handlebars<'static>, dir: &Path, prefix: &str) -> ClResult<()> {
	for path in read_dir(dir)? {
		let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
		if path.is_dir() {
			load_dir(hb, &path, &format!("{prefix}/{name}"))?;
		} else if let Some(stem) = name.strip_suffix(".md")
			&& stem.contains('.')
		{
			let src = std::fs::read_to_string(&path).map_err(|e| io(&path, &e))?;
			hb.register_template_string(&format!("{prefix}/{stem}"), src)
				.map_err(|e| Error::internal(format!("prompt {}: {e}", path.display())))?;
		}
	}
	Ok(())
}

fn io(path: &Path, e: &std::io::Error) -> Error {
	Error::internal(format!("prompt dir {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn falls_back_to_en_and_pins_language() {
		let root = std::env::temp_dir().join(format!("saas-llm-prompts-{}", std::process::id()));
		let dir = root.join("packs/research/prompts/plan");
		std::fs::create_dir_all(&dir).unwrap();
		std::fs::write(dir.join("outline.en.md"), "Outline {{topic}} & more ({{lang}})").unwrap();
		std::fs::write(dir.join("outline.hu.md"), "Vázlat: {{topic}}").unwrap();
		let p = Prompts::load(&root).unwrap();
		std::fs::remove_dir_all(&root).unwrap();

		let de = p.render("research/plan/outline", "de", &json!({"topic": "x"})).unwrap();
		assert!(de.starts_with("Outline x & more (de)"), "{de}");
		assert!(de.ends_with("code `de`."), "{de}");
		let hu = p.render("research/plan/outline", "hu", &json!({"topic": "x"})).unwrap();
		assert!(hu.starts_with("Vázlat: x"), "{hu}");
		let hu = p.render("research/plan/outline", "hu-HU", &json!({"topic": "x"})).unwrap();
		assert!(hu.starts_with("Vázlat: x") && hu.ends_with("code `hu-HU`."), "{hu}");
		assert!(p.render("research/plan/missing", "en", &Value::Null).is_err());
		// Strict mode: a missing variable is an error, not an empty string.
		assert!(p.render("research/plan/outline", "en", &Value::Null).is_err());
	}
}

// vim: ts=4
