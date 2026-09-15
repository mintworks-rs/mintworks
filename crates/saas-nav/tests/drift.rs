//! Vendored-schema drift check. This is the only test that touches the network, so it is
//! `#[ignore]`d: run it on a schedule with
//! `cargo test -p saas-nav --test drift -- --ignored`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::{fs, process::Command};

const XSD_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/xsd");

/// Vendored file name -> published URL. Kept in step with `xsd/README.md`.
const SOURCES: &[(&str, &str)] = &[
	(
		"invoiceApi.xsd",
		"https://raw.githubusercontent.com/nav-gov-hu/Online-Invoice/master/src/schemas/nav/gov/hu/OSA/invoiceApi.xsd",
	),
	(
		"invoiceData.xsd",
		"https://raw.githubusercontent.com/nav-gov-hu/Online-Invoice/master/src/schemas/nav/gov/hu/OSA/invoiceData.xsd",
	),
	(
		"invoiceBase.xsd",
		"https://raw.githubusercontent.com/nav-gov-hu/Online-Invoice/master/src/schemas/nav/gov/hu/OSA/invoiceBase.xsd",
	),
	(
		"invoiceAnnulment.xsd",
		"https://raw.githubusercontent.com/nav-gov-hu/Online-Invoice/master/src/schemas/nav/gov/hu/OSA/invoiceAnnulment.xsd",
	),
	(
		"serviceMetrics.xsd",
		"https://raw.githubusercontent.com/nav-gov-hu/Online-Invoice/master/src/schemas/nav/gov/hu/OSA/serviceMetrics.xsd",
	),
	(
		"common.xsd",
		"https://raw.githubusercontent.com/nav-gov-hu/Common/Common-1.0.RC3/src/schemas/nav/gov/hu/NTCA/common.xsd",
	),
];

#[test]
#[ignore = "downloads the published XSDs; the only network-touching test"]
fn vendored_schemas_match_published() {
	let tmp = std::env::temp_dir().join(format!("saas-nav-xsd-drift-{}", std::process::id()));
	fs::create_dir_all(&tmp).unwrap();
	let mut drifted = Vec::new();
	for (name, url) in SOURCES {
		let fetched = tmp.join(name);
		// curl + diff instead of an HTTP client dev-dependency; this test is
		// manual and scheduled, not part of `cargo test`.
		let ok = Command::new("curl")
			.args(["-sfL", "--max-time", "60", "-o"])
			.arg(&fetched)
			.arg(url)
			.status()
			.unwrap()
			.success();
		assert!(ok, "could not download {url}");
		let out = Command::new("diff")
			.args(["-u", "--label", "vendored", "--label", "published"])
			.arg(format!("{XSD_DIR}/{name}"))
			.arg(&fetched)
			.output()
			.unwrap();
		if !out.status.success() {
			drifted.push(format!("{name}:\n{}", String::from_utf8_lossy(&out.stdout)));
		}
	}
	assert!(
		drifted.is_empty(),
		"vendored XSDs drifted from the published ones:\n{}",
		drifted.join("\n")
	);
}

// vim: ts=4
