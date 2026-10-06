// SPDX-License-Identifier: MPL-2.0
//! Classifying a response and grouping the mismatches of one layer into a report file.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Instant;

use axum::http::StatusCode;

use crate::fixture::Resp;
use crate::oracle::{Expect, Outcome};

/// The errCodes that mean authorization refused. Any other 4xx means it passed: a 400 from an
/// unauthorized caller is validation before authz, which leaks, so it must read as Allow.
const AUTHZ: [&str; 11] = [
	"E-AUTH-TOKEN",
	"E-AUTH-KEY-REVOKED",
	"E-AUTH-PENDING",
	"E-AUTH-SUSPENDED",
	"E-AUTH-ANONYMIZED",
	"E-AUTH-SCOPE",
	"E-AUTH-CONSENT-REQUIRED",
	"E-AUTH-FORBIDDEN",
	"E-AUTH-STEPUP",
	"E-AUTH-STEPUP-IMPOSSIBLE",
	"E-CORE-NOTFOUND",
];

/// `Err` is a harness error: never a policy outcome, never reclassified as a mismatch.
pub fn classify(r: &Resp) -> Result<Outcome, String> {
	let status = r.status;
	if status.is_success() {
		return Ok(Outcome::Allow);
	}
	if r.empty {
		return Err(format!("{} with an empty body (route not mounted?)", status.as_u16()));
	}
	let code = r.body.as_ref().and_then(|b| b["error"]["errCode"].as_str());
	let Some(code) = code else {
		return Err(format!("{} without an errCode", status.as_u16()));
	};
	if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
		return Err(format!("{} {code}", status.as_u16()));
	}
	Ok(AUTHZ.iter().find(|c| **c == code).map_or(Outcome::Allow, |c| Outcome::Deny(c)))
}

pub struct Mismatch {
	pub layer: &'static str,
	pub route: String,
	pub rule: &'static str,
	pub expected: String,
	pub actual: String,
	pub subject: String,
	pub object: Option<String>,
}

#[derive(Default)]
struct Group {
	count: usize,
	samples: Vec<String>,
}

impl Group {
	fn add(&mut self, sample: String) {
		self.count += 1;
		if self.samples.len() < 5 {
			self.samples.push(sample);
		}
	}
}

fn sample(subject: &str, object: Option<&str>) -> String {
	format!("{subject} × {}", object.unwrap_or("-"))
}

fn show(o: Outcome) -> String {
	match o {
		Outcome::Allow => "Allow".into(),
		Outcome::Deny(code) => code.into(),
	}
}

pub struct Report {
	layer: &'static str,
	started: Instant,
	cells: usize,
	/// `(route, rule, expected, actual)`.
	mismatches: BTreeMap<(String, &'static str, String, String), Group>,
	/// `(route, detail)`.
	errors: BTreeMap<(String, String), Group>,
}

impl Report {
	pub fn new(layer: &'static str, planned: usize) -> Self {
		eprintln!("[{layer}] start: {planned} cells planned");
		Report {
			layer,
			started: Instant::now(),
			cells: 0,
			mismatches: BTreeMap::new(),
			errors: BTreeMap::new(),
		}
	}

	/// Judges one cell. `seen` is the object's presence in a listing, when the cell paged one.
	pub fn check(
		&mut self,
		route: &str,
		subject: &str,
		object: Option<&str>,
		exp: &Expect,
		resp: &Resp,
		seen: Option<bool>,
	) {
		self.cells += 1;
		let actual = match classify(resp) {
			Ok(a) => a,
			Err(detail) => return self.error(route, &detail, subject, object),
		};
		let mismatch = |expected: String, actual: String| Mismatch {
			layer: self.layer,
			route: route.into(),
			rule: exp.rule,
			expected,
			actual,
			subject: subject.into(),
			object: object.map(Into::into),
		};
		if actual != exp.outcome {
			let m = mismatch(show(exp.outcome), show(actual));
			return self.mismatch(m);
		}
		if let (Some(want), Some(got)) = (exp.listed, seen)
			&& want != got
		{
			let m = mismatch(format!("listed={want}"), format!("listed={got}"));
			self.mismatch(m);
		}
	}

	pub fn mismatch(&mut self, m: Mismatch) {
		let key = (m.route, m.rule, m.expected, m.actual);
		self.mismatches
			.entry(key)
			.or_default()
			.add(sample(&m.subject, m.object.as_deref()));
	}

	pub fn error(&mut self, route: &str, detail: &str, subject: &str, object: Option<&str>) {
		let key = (route.to_string(), detail.to_string());
		self.errors.entry(key).or_default().add(sample(subject, object));
	}

	/// Writes `$CARGO_TARGET_TMPDIR/access-matrix-<layer>.md`, then panics once if anything
	/// failed.
	pub fn finish(self) {
		let ms = self.started.elapsed().as_millis().max(1);
		let rate = self.cells as u128 * 1000 / ms;
		eprintln!("[{}] done: {} cells in {ms} ms ({rate} cells/s)", self.layer, self.cells);

		let mut md = format!("# Access matrix — {}\n\n{} cells\n", self.layer, self.cells);
		let n_mis: usize = self.mismatches.values().map(|g| g.count).sum();
		let n_err: usize = self.errors.values().map(|g| g.count).sum();
		let _ = write!(md, "\n## Harness errors ({n_err} in {} groups)\n\n", self.errors.len());
		for ((route, detail), g) in &self.errors {
			let _ = writeln!(md, "- `{route}` — {detail} ×{}: {}", g.count, g.samples.join("; "));
		}
		let groups = self.mismatches.len();
		let _ = write!(md, "\n## Mismatches ({n_mis} in {groups} groups)\n\n");
		for ((route, rule, exp, act), g) in &self.mismatches {
			let _ = writeln!(
				md,
				"- `{route}` [{rule}] expected {exp}, got {act} ×{}: {}",
				g.count,
				g.samples.join("; ")
			);
		}
		let path = format!("{}/access-matrix-{}.md", env!("CARGO_TARGET_TMPDIR"), self.layer);
		std::fs::write(&path, md).unwrap();

		assert!(
			self.errors.is_empty() && self.mismatches.is_empty(),
			"[{}] {n_err} harness errors in {} groups, {n_mis} mismatches in {groups} groups — see {path}",
			self.layer,
			self.errors.len(),
		);
	}
}

// vim: ts=4
