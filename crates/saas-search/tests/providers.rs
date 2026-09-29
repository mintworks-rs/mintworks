//! Direct fetch against a loopback server, the review-site block and allow-list, HTML
//! extraction, and the `fake` fixtures. The engines' own tests live in their adapters.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use saas_core::Error;
use saas_search::{
	Direct, E_BLOCKED, E_FAKE_EMPTY, E_NOT_DIRECT, Endpoint, Fetcher, Fixtures, Page, SearchHit,
	SearchProvider,
	direct::{check_fetchable, html_to_text},
};
use wiremock::{
	Mock, MockServer, ResponseTemplate,
	matchers::{method, path},
};

fn code(e: &Error) -> &'static str {
	e.parts().1
}

#[test]
fn review_sites_are_never_fetched() {
	for url in [
		"https://www.g2.com/products/x/reviews",
		"https://www.capterra.de/software/1",
		"https://uk.trustpilot.com/review/example.com",
	] {
		let e = check_fetchable(url).unwrap_err();
		assert_eq!(code(&e), E_BLOCKED, "{url}");
	}
	assert!(check_fetchable("https://example.com/g2").is_ok());
	assert!(check_fetchable("https://nav.gov.hu/").is_ok());
	assert!(matches!(check_fetchable("ftp://example.com/"), Err(Error::Validation(_))));
	assert!(matches!(check_fetchable("not a url"), Err(Error::Validation(_))));
}

#[test]
fn direct_allow_list_covers_subdomains_only() {
	let d = Direct::from_csv(" nav.gov.hu, Example.org. ,");
	assert_eq!(d.domains, ["nav.gov.hu", "example.org"]);
	assert!(d.covers("https://nav.gov.hu/afa"));
	assert!(d.covers("https://www.nav.gov.hu/afa"));
	assert!(d.covers("http://EXAMPLE.org/"));
	assert!(!d.covers("https://evilnav.gov.hu/"));
	assert!(!d.covers("https://nav.gov.hu.evil.com/"));
	assert!(!Direct::from_csv("").covers("https://nav.gov.hu/"));
}

#[tokio::test]
async fn direct_fetch_extracts_html_and_refuses_outside_the_list() {
	let server = MockServer::start().await;
	Mock::given(method("GET"))
		.and(path("/afa"))
		.respond_with(ResponseTemplate::new(200).set_body_raw(
			"<!doctype html><html><head><title>Áfa &amp; kulcsok</title></head>\
			 <body><p>Általános kulcs: 27%</p><script>x()</script></body></html>",
			"text/html",
		))
		.mount(&server)
		.await;
	let mut d = Direct::from_csv("127.0.0.1");
	d.allow_internal = true;
	let page = d.fetch(&format!("{}/afa", server.uri())).await.unwrap();
	assert_eq!(
		(page.title.as_str(), page.text.as_str()),
		("Áfa & kulcsok", "Általános kulcs: 27%")
	);

	let e = d.fetch("https://example.com/").await.unwrap_err();
	assert_eq!(code(&e), E_NOT_DIRECT);
	let e = Direct::from_csv("g2.com").fetch("https://g2.com/").await.unwrap_err();
	assert_eq!(code(&e), E_BLOCKED);
}

#[tokio::test]
async fn direct_fetch_is_ssrf_guarded_without_allow_internal() {
	let e = Direct::from_csv("127.0.0.1").fetch("http://127.0.0.1:9/").await.unwrap_err();
	assert!(matches!(e, Error::Validation(_)), "{e:?}");
}

#[test]
fn html_to_text_drops_markup() {
	let (title, text) = html_to_text(
		"<HTML><HEAD><TITLE> A\n title </TITLE><style>p{}</style></HEAD><BODY>\
		 <h1>Head</h1><!-- a > comment --><div>one <b>two</b><br/>three</div>\
		 <ul><li>x &lt; y</li><li>&quot;z&quot;&nbsp;w</li></ul></BODY></HTML>",
	);
	assert_eq!(title, "A title");
	assert_eq!(text, "Head\none two\nthree\nx < y\n\"z\" w");
}

#[tokio::test]
async fn fake_serves_fixtures() {
	let fx = Fixtures::default();
	let ep = Endpoint::default();
	let hit =
		SearchHit { title: "t".into(), url: "https://a.example/".into(), snippet: "s".into() };
	fx.push_search(vec![hit.clone()]);
	fx.push_search(vec![]);
	fx.put_page(Page {
		url: hit.url.clone(),
		title: "A".into(),
		text: "body".into(),
		tokens: None,
	});

	assert_eq!(fx.search(&ep, "q", 5).await.unwrap().hits, std::slice::from_ref(&hit));
	assert!(fx.search(&ep, "q", 5).await.unwrap().hits.is_empty());
	assert_eq!(code(&fx.search(&ep, "q", 5).await.unwrap_err()), E_FAKE_EMPTY);
	// A page stays: an agent may fetch it twice.
	for _ in 0..2 {
		assert_eq!(fx.fetch(&ep, &hit.url).await.unwrap().text, "body");
	}
	assert_eq!(code(&fx.fetch(&ep, "https://b.example/").await.unwrap_err()), E_FAKE_EMPTY);
	fx.clear();
	assert_eq!(code(&fx.fetch(&ep, &hit.url).await.unwrap_err()), E_FAKE_EMPTY);
	assert_eq!((SearchProvider::id(&fx), Fetcher::id(&fx)), ("fake", "fake"));
}

// vim: ts=4
