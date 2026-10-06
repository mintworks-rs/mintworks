// SPDX-License-Identifier: MPL-2.0
//! The table against the code, both ways: every `RouteSpec` is mounted, and every literal
//! `.route(` under `crates/*/src` has a `RouteSpec`. Checks the table, not policy.

use std::collections::BTreeSet;
use std::path::Path;

use axum::http::{Method, StatusCode};
use serde_json::json;

use crate::fixture::{Fixture, call, req};
use crate::routes::routes;
use crate::subjects::clone_subject;

/// Script routes are declared at runtime, not by literal.
const SKIP: &[&str] = &["crates/script/src/routes.rs"];

pub async fn check(fx: &'static Fixture) {
	let specs: BTreeSet<(String, String)> = routes()
		.iter()
		.map(|s| (s.method.as_str().to_owned(), s.path.to_owned()))
		.collect();
	let mut problems = Vec::new();

	// (a) Mounted: as a disposable owner of A, with every path parameter a placeholder that
	// names no real object and a `{}` body, so no row can mutate shared fixture state.
	let owner = clone_subject(fx, fx.subject("owner_a")).await;
	for s in routes() {
		let uri = placeholder(s.path);
		let body = (s.method != Method::GET && s.method != Method::DELETE).then(|| json!({}));
		let r = call(&fx.router, req(s.method.clone(), &uri, owner.bearer.as_deref(), body)).await;
		if r.empty && matches!(r.status, StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED) {
			problems.push(format!("not mounted: {} {} ({})", s.method, s.path, r.status));
		}
	}

	// (b) Specified: a source scan of the bundle files.
	let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
	let mut scanned = BTreeSet::new();
	let mut files = Vec::new();
	rs_files(&root.join("crates"), &mut files);
	for f in files {
		let rel = f.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
		if SKIP.contains(&rel.as_str()) {
			continue;
		}
		if cfg!(not(feature = "ai")) && rel.starts_with("crates/agent/") {
			continue;
		}
		scanned.extend(scan(&rel, &std::fs::read_to_string(&f).unwrap(), &mut problems));
	}
	for (m, p) in scanned.difference(&specs) {
		problems.push(format!("no RouteSpec: {m} {p}"));
	}
	for (m, p) in specs.difference(&scanned) {
		problems.push(format!("RouteSpec matches no literal .route(: {m} {p}"));
	}

	assert!(problems.is_empty(), "route table drift:\n  {}", problems.join("\n  "));
}

fn placeholder(path: &str) -> String {
	path.split('/')
		.map(|seg| if seg.starts_with('{') { "drift-none" } else { seg })
		.collect::<Vec<_>>()
		.join("/")
}

fn rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
	for e in std::fs::read_dir(dir).unwrap() {
		let p = e.unwrap().path();
		if p.is_dir() {
			rs_files(&p, out);
		} else if p.extension().is_some_and(|x| x == "rs")
			&& p.components().any(|c| c.as_os_str() == "src")
		{
			out.push(p);
		}
	}
}

/// `(METHOD, path)` for every `.route("<path>", …)` outside `#[cfg(test)]` items. The path is
/// the first string literal; the methods are `get(`/`post(`/… anywhere inside the call. What it
/// cannot model — a non-literal path, a `.nest(` prefix — goes to `problems`, never skipped.
fn scan(rel: &str, src: &str, problems: &mut Vec<String>) -> Vec<(String, String)> {
	let src = &strip_cfg_test(src);
	if src.contains(".nest(") {
		problems.push(format!(
			"{rel}: .nest( prefix not modelled — list it by hand or add the file to SKIP"
		));
	}
	let b = src.as_bytes();
	let mut out = Vec::new();
	let mut from = 0;
	while let Some(i) = src[from..].find(".route(") {
		let start = from + i + ".route(".len();
		from = start;
		if !src[start..].trim_start().starts_with('"') {
			problems.push(format!(
				"{rel}: non-literal .route( — list it by hand or add the file to SKIP"
			));
			continue;
		}
		let (mut depth, mut j, mut in_str, mut lit, mut path) = (1, start, false, 0, None);
		while j < b.len() && depth > 0 {
			match (in_str, b[j]) {
				(true, b'\\') => j += 1,
				(true, b'"') => {
					in_str = false;
					path.get_or_insert(&src[lit..j]);
				}
				(false, b'"') => (in_str, lit) = (true, j + 1),
				(false, b'(') => depth += 1,
				(false, b')') => depth -= 1,
				_ => {}
			}
			j += 1;
		}
		let args = &src[start..j];
		for m in ["get", "post", "put", "patch", "delete"] {
			let pat = format!("{m}(");
			let hits = args.match_indices(&pat).filter(|(k, _)| {
				args[..*k]
					.chars()
					.last()
					.is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == ':'))
			});
			for _ in hits {
				out.push((m.to_uppercase(), path.unwrap_or_default().to_owned()));
			}
		}
	}
	out
}

/// `src` without the item after each `#[cfg(test)]`: through its matching `}` when a `{` opens
/// before the next `;`, else through that `;`. String literals are skipped when counting braces.
fn strip_cfg_test(src: &str) -> String {
	const ATTR: &str = "#[cfg(test)]";
	let b = src.as_bytes();
	let mut out = String::new();
	let mut rest = 0;
	while let Some(i) = src[rest..].find(ATTR) {
		let at = rest + i;
		out.push_str(&src[rest..at]);
		let mut j = at + ATTR.len();
		let (mut depth, mut in_str) = (0usize, false);
		while j < b.len() {
			match (in_str, b[j]) {
				(true, b'\\') => j += 1,
				(true, b'"') => in_str = false,
				(false, b'"') => in_str = true,
				(false, b'\'') if b.get(j + 2) == Some(&b'\'') => j += 2,
				(false, b'{') => depth += 1,
				(false, b'}') => {
					depth -= 1;
					if depth == 0 {
						break;
					}
				}
				(false, b';') if depth == 0 => break,
				_ => {}
			}
			j += 1;
		}
		rest = (j + 1).min(src.len());
	}
	out.push_str(&src[rest..]);
	out
}

#[test]
fn strip_cfg_test_keeps_code_after_a_test_module() {
	let src = "a\n#[cfg(test)]\nmod t { fn f() { let _ = \"}\"; let _ = '{'; } }\nb\n#[cfg(test)]\nuse x;\nc";
	assert_eq!(strip_cfg_test(src).split_whitespace().collect::<Vec<_>>(), ["a", "b", "c"]);
}

// vim: ts=4
