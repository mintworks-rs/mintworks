//! Direct fetch for the `search.direct_domains` allow-list, through saas-core's external client
//! and its `NoInternal` resolver, plus the review-site block every fetch path shares.

use std::time::Duration;

use saas_core::{ClResult, Error, error::StatusCode, http};

use crate::{E_BLOCKED, E_NOT_DIRECT, Page, status_error};

/// Review sites: never fetched, directly or through the fetcher — only their search snippets
/// may be quoted. Matched as a host label so every country domain (`capterra.de`) is covered.
pub const BLOCKED_SITES: &[&str] = &["capterra", "g2", "trustpilot"];

/// The lowercased host of an `http`/`https` URL.
pub fn host(url: &str) -> ClResult<String> {
	let uri = url
		.parse::<hyper::Uri>()
		.map_err(|e| Error::Validation(format!("not a URL: {url}: {e}")))?;
	match uri.scheme_str() {
		Some("http" | "https") => {}
		_ => return Err(Error::Validation(format!("not an http(s) URL: {url}"))),
	}
	match uri.host() {
		Some(h) if !h.is_empty() => Ok(h.trim_end_matches('.').to_ascii_lowercase()),
		_ => Err(Error::Validation(format!("URL has no host: {url}"))),
	}
}

/// Refuses a malformed URL or a review site. The TLD label is skipped, so `g2` never matches
/// a host ending in a `.g2` TLD it does not name.
pub fn check_fetchable(url: &str) -> ClResult<()> {
	let host = host(url)?;
	let mut labels: Vec<&str> = host.split('.').collect();
	labels.pop();
	if labels.iter().any(|l| BLOCKED_SITES.contains(l)) {
		return Err(Error::coded(
			StatusCode::FORBIDDEN,
			E_BLOCKED,
			format!("{host} is a review site: quote its search snippet instead of fetching it"),
		));
	}
	Ok(())
}

/// `host` is `domain` or one of its subdomains.
fn under(host: &str, domain: &str) -> bool {
	host == domain || host.strip_suffix(domain).is_some_and(|rest| rest.ends_with('.'))
}

#[derive(Clone, Debug)]
pub struct Direct {
	/// Lowercased hosts; each covers its subdomains too.
	pub domains: Vec<String>,
	pub deadline: Duration,
	/// Let the target be a loopback or private address (a test server).
	pub allow_internal: bool,
}

impl Direct {
	/// From the `search.direct_domains` CSV.
	pub fn from_csv(csv: &str) -> Self {
		Self {
			domains: csv
				.split(',')
				.map(|d| d.trim().trim_end_matches('.').to_ascii_lowercase())
				.filter(|d| !d.is_empty())
				.collect(),
			deadline: Duration::from_secs(30),
			allow_internal: false,
		}
	}

	/// Whether `url` is routed here rather than to the fetcher.
	pub fn covers(&self, url: &str) -> bool {
		host(url).is_ok_and(|h| self.domains.iter().any(|d| under(&h, d)))
	}

	/// A redirect is not followed: it answers as a refusal, so it cannot leave the allow-list.
	pub async fn fetch(&self, url: &str) -> ClResult<Page> {
		check_fetchable(url)?;
		if !self.covers(url) {
			return Err(Error::coded(
				StatusCode::FORBIDDEN,
				E_NOT_DIRECT,
				format!("{url} is not in search.direct_domains"),
			));
		}
		let (status, _, bytes) = http::get_external(
			url,
			&[("accept", "text/html, text/plain;q=0.9")],
			self.deadline,
			self.allow_internal,
		)
		.await?;
		if !status.is_success() {
			return Err(status_error("direct", url, status));
		}
		let body = String::from_utf8_lossy(&bytes);
		let (title, text) = if body.trim_start().starts_with('<') {
			html_to_text(&body)
		} else {
			(String::new(), body.into_owned())
		};
		Ok(Page { url: url.to_owned(), title, text, tokens: None })
	}
}

const SKIPPED: &[&str] = &["head", "script", "style", "noscript", "template", "svg"];
const BLOCK: &[&str] = &[
	"p",
	"br",
	"div",
	"li",
	"tr",
	"h1",
	"h2",
	"h3",
	"h4",
	"h5",
	"h6",
	"section",
	"article",
	"header",
	"footer",
	"table",
	"ul",
	"ol",
	"blockquote",
	"pre",
	"hr",
];

/// `(title, text)` of an HTML page: tags dropped, block tags as line breaks, the common named
/// entities decoded.
// No DOM, no numeric entities; the allow-listed sites are plain government pages.
// Reach for an HTML parser crate if one needs more.
pub fn html_to_text(html: &str) -> (String, String) {
	// ASCII lowercasing keeps byte offsets, so indices into `lower` slice `html` too.
	let lower = html.to_ascii_lowercase();
	let title = lower
		.find("<title")
		.and_then(|s| Some(s + lower[s..].find('>')? + 1))
		.and_then(|s| Some((s, s + lower[s..].find("</title")?)))
		.map(|(s, e)| collapse(&decode(&html[s..e])))
		.unwrap_or_default();

	let mut out = String::with_capacity(html.len() / 2);
	let mut i = 0;
	while let Some(lt) = html[i..].find('<') {
		out.push_str(&html[i..i + lt]);
		let start = i + lt;
		if lower[start..].starts_with("<!--") {
			i = lower[start..].find("-->").map_or(html.len(), |e| start + e + 3);
			continue;
		}
		let Some(gt) = html[start..].find('>') else {
			i = html.len();
			break;
		};
		let tag = &lower[start + 1..start + gt];
		let name = tag
			.trim_start_matches('/')
			.split(|c: char| c.is_ascii_whitespace() || c == '/')
			.next()
			.unwrap_or_default();
		i = start + gt + 1;
		if !tag.starts_with('/') && SKIPPED.contains(&name) && !tag.ends_with('/') {
			i = lower[i..].find(&format!("</{name}")).map_or(html.len(), |e| i + e);
			continue;
		}
		out.push(if BLOCK.contains(&name) { '\n' } else { ' ' });
	}
	out.push_str(&html[i..]);
	(
		title,
		decode(&out)
			.lines()
			.map(collapse)
			.filter(|l| !l.is_empty())
			.collect::<Vec<_>>()
			.join("\n"),
	)
}

fn decode(s: &str) -> String {
	s.replace("&nbsp;", " ")
		.replace("&lt;", "<")
		.replace("&gt;", ">")
		.replace("&quot;", "\"")
		.replace("&#39;", "'")
		.replace("&apos;", "'")
		.replace("&amp;", "&")
}

fn collapse(s: &str) -> String {
	s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// vim: ts=4
