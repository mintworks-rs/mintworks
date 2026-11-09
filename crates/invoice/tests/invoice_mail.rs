// SPDX-License-Identifier: MPL-2.0
//! Invoice email through the `Invoices` service handle and the `INVOICE_MAIL` job against a real
//! `SqliteStore`: the automatic send and its opt-out, the manual resend and its caps, the PDF
//! wait, the hand-off to `SEND_EMAIL`, the settle sweep and the mail-problem filter.
//!
//! The raw-trait half (`mail_queue`'s caps and dedup) is in `testing/store-conformance`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mintworks_core::job::{Job, Next};
use mintworks_core::{App, AppBuilder, config::Config, ctx::Ctx, ids::SellerId, prelude::*};
use mintworks_invoice::{
	draft::{Line, NewDraft, Party},
	mail,
	service_api::Invoices,
	store::{
		Invoice, InvoiceFilter, InvoiceMail, InvoiceStore, MailReason, MailStatus, Seller,
		SellerVersionPatch,
	},
	vat::VatCode,
};
use mintworks_store_sqlite::SqliteStore;

const ORG: i64 = 1;
const ROOT: i64 = 0;
const SELLER: i64 = 1;

/// A temp directory that takes the database with it.
struct TmpDb(std::path::PathBuf);

impl TmpDb {
	fn new(name: &str) -> Self {
		let dir = std::env::temp_dir()
			.join(format!("mintworks-invoice-mail-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		Self(dir)
	}

	fn config(&self) -> Config {
		Config {
			master_key: [0; 32],
			db_path: self.0.join("test.db").to_string_lossy().into_owned(),
			data_dir: self.0.to_string_lossy().into_owned(),
			listen: String::new(),
			base_url: "https://app.test".into(),
			jobs_workers: None,
		}
	}
}

impl Drop for TmpDb {
	fn drop(&mut self) {
		let _ = std::fs::remove_dir_all(&self.0);
	}
}

struct Fx {
	_db: TmpDb,
	app: App,
	inv: Invoices,
	store: SqliteStore,
	sys: Ctx,
}

impl Fx {
	async fn sql(&self, q: &str) {
		sqlx::query(sqlx::AssertSqlSafe(q.to_owned()))
			.execute(self.store.write_pool())
			.await
			.unwrap();
	}

	async fn set(&self, key: &str, v: &str) {
		self.app.settings.set_at(ORG, 0, key, Some(v), None).await.unwrap();
	}

	/// An issued invoice to the default party; issuing queues its mail.
	async fn issued(&self) -> Invoice {
		let d = self.draft().await;
		self.inv.issue(&self.sys, d.uid.as_str()).await.unwrap()
	}

	/// An invoice issued without sending, its opt-out cleared, so `mail_auto` is the first send.
	async fn issued_unsent(&self) -> Invoice {
		let d = self.draft().await;
		let inv = self.inv.issue_with(&self.sys, d.uid.as_str(), Some(false)).await.unwrap();
		self.inv.set_mail_optout(&self.sys, inv.uid.as_str(), false).await.unwrap();
		inv
	}

	/// The invoice's newest mail row.
	async fn row_of(&self, inv: &Invoice) -> InvoiceMail {
		self.store.mail_rows(ORG, inv.id).await.unwrap().remove(0)
	}

	async fn draft(&self) -> Invoice {
		self.inv
			.draft(
				&self.sys,
				&NewDraft {
					billing_party: Party::OrgDefault,
					lines: vec![Line {
						code: None,
						description: "Tanacsadas".into(),
						unit: "ora".into(),
						qty: Qty(1_000_000),
						unit_price: Some(Money(100_000)),
						vat_code: Some(VatCode::Std27),
						discount: None,
						discount_description: None,
						note: None,
					}],
					..NewDraft::default()
				},
			)
			.await
			.unwrap()
	}

	async fn with_pdf(&self, inv: &Invoice) {
		self.sql(&format!(
			"INSERT INTO invoice_documents (invoice_id, kind, sha256, bytes, template_version, rendered_at)
			 VALUES ({}, 'PDF', 'f00d', 1, 't', 0)",
			inv.id
		))
		.await;
	}

	async fn rows(&self, inv: &Invoice) -> Vec<(MailStatus, Option<MailReason>)> {
		let rows = self.inv.mails(&self.sys, inv.uid.as_str()).await.unwrap();
		rows.iter().map(|r| (r.status, r.reason)).collect()
	}

	/// `(dedup_key, payload)` of every job of `kind`, oldest first.
	async fn jobs(&self, kind: &str) -> Vec<(String, serde_json::Value)> {
		let rows: Vec<(Option<String>, String)> =
			sqlx::query_as("SELECT dedup_key, payload FROM jobs WHERE kind = ? ORDER BY id")
				.bind(kind)
				.fetch_all(self.store.write_pool())
				.await
				.unwrap();
		rows.into_iter()
			.map(|(k, p)| (k.unwrap_or_default(), serde_json::from_str(&p).unwrap()))
			.collect()
	}

	/// Runs the `INVOICE_MAIL` job of `uid` as its `attempts`-th try.
	async fn deliver(&self, uid: &str, attempts: i64) -> Next {
		let payload = serde_json::json!({ "orgId": ORG, "invoiceId": 0, "mailUid": uid });
		let job =
			Job { id: 0, kind: mail::KIND_MAIL.into(), payload: payload.to_string(), attempts };
		mail::deliver(&self.app, &self.store, &job).await.unwrap()
	}

	fn problems() -> InvoiceFilter {
		InvoiceFilter { mail_problem: true, ..InvoiceFilter::default() }
	}

	async fn problem_count(&self) -> u64 {
		self.inv.list_page(&self.sys, &Self::problems(), None, 50).await.unwrap().1
	}
}

async fn fixture(name: &str) -> Fx {
	let db = TmpDb::new(name);
	let store = SqliteStore::open(&db.config()).await.unwrap();
	store.migrate(&[mintworks_store_sqlite::FRAMEWORK]).await.unwrap();
	let app = AppBuilder::new()
		.config(db.config())
		.store(std::sync::Arc::new(store.clone())
			as std::sync::Arc<dyn mintworks_core::store::CoreStore>)
		.settings(mintworks_invoice::SETTINGS)
		.extension(std::sync::Arc::new(store.clone()) as std::sync::Arc<dyn InvoiceStore>)
		.extension(std::sync::Arc::new(store.clone())
			as std::sync::Arc<dyn mintworks_core::refs::RefStore>)
		.build()
		.await
		.unwrap();
	let fx = Fx {
		_db: db,
		inv: Invoices::new(app.clone()),
		app,
		store,
		sys: Ctx::system("test").with_org(ORG),
	};
	fx.sql("INSERT INTO accounts (id, uid, email, created_at) VALUES (1, 'acc_t', 't@e.st', 0), (2, 'acc_m', 'm@e.st', 0)").await;
	fx.sql(&format!("UPDATE orgs SET id = {ROOT} WHERE kind = 'ROOT'")).await;
	fx.sql(&format!(
		"INSERT INTO orgs (id, uid, parent_id, kind, name, owner_account_id, created_at)
		 VALUES ({ORG}, 'org_t', (SELECT id FROM orgs WHERE kind = 'ROOT'), 'SHARED', 'Teszt', 1, 0)"
	))
	.await;
	fx.sql(&format!(
		"INSERT INTO billing_parties
		 (id, uid, org_id, kind, name, country, tax_number, postcode, city, street, email, emails,
		  is_default, created_at, updated_at)
		 VALUES (1, 'prt_01JCZ5X8K9N7QW3M6R2T4V8Y0B', {ORG}, 'C', 'Vevo Zrt.', 'HU', '87654321242',
		  '1052', 'Budapest', 'Deak ter 2.', 'a@vevo.hu', '[\"a@vevo.hu\",\"b@vevo.hu\",\"A@VEVO.HU\"]',
		  1, 0, 0)"
	))
	.await;
	fx.store
		.put_seller(&Seller {
			id: SELLER,
			uid: SellerId::generate(),
			org_id: ORG,
			nav_base_url: "https://api-test.onlineszamla.nav.gov.hu".into(),
			nav_login: None,
			series_code: "A".into(),
			test_mode: false,
			closed_at: None,
			payment_days: None,
			created_at: Timestamp::now(),
		})
		.await
		.unwrap();
	let ver = SellerVersionPatch {
		name: Some("Teszt Kft.".into()),
		country: Some("HU".into()),
		tax_number: Some("12345678242".into()),
		postcode: Some("1011".into()),
		city: Some("Budapest".into()),
		street: Some("Fo utca 1.".into()),
		vat_scheme: Some("NORMAL".into()),
		email: Patch::Value(" info@teszt.hu ".into()),
		..Default::default()
	};
	fx.store.save_seller_version_draft(SELLER, &ver).await.unwrap();
	fx.store
		.publish_seller_version(SELLER, Timestamp::now(), &|_| Ok(()))
		.await
		.unwrap();
	fx
}

#[tokio::test]
async fn issuing_queues_the_mail_once_with_its_job() {
	let fx = fixture("auto-once").await;
	let inv = fx.issued().await;
	let row = fx.row_of(&inv).await;
	assert_eq!((row.status, row.recipient.as_str()), (MailStatus::Queued, "a@vevo.hu"));
	assert_eq!(row.cc, ["b@vevo.hu"]);
	assert!(fx.inv.mail_auto(&fx.sys, inv.uid.as_str()).await.unwrap().is_none());
	assert_eq!(fx.rows(&inv).await, [(MailStatus::Queued, None)]);
	let jobs = fx.jobs(mail::KIND_MAIL).await;
	assert_eq!(jobs.len(), 1);
	assert_eq!(jobs[0].0, format!("invmail:{}", row.uid.as_str()));
	assert_eq!(jobs[0].1["mailUid"], row.uid.as_str());
}

#[tokio::test]
async fn auto_mail_off_issues_quietly_unless_the_caller_asks() {
	let fx = fixture("auto-off").await;
	fx.set("invoice.mail.auto", "0").await;
	let quiet = fx.issued().await;
	assert!(fx.rows(&quiet).await.is_empty());
	let d = fx.draft().await;
	let sent = fx.inv.issue_with(&fx.sys, d.uid.as_str(), Some(true)).await.unwrap();
	assert_eq!(fx.rows(&sent).await, [(MailStatus::Queued, None)]);
}

#[tokio::test]
async fn the_mail_names_the_seller_the_invoice_was_issued_under() {
	let fx = fixture("frozen-seller").await;
	let inv = fx.issued().await;
	let renamed = SellerVersionPatch { name: Some("Uj Nev Kft.".into()), ..Default::default() };
	fx.store.save_seller_version_draft(SELLER, &renamed).await.unwrap();
	fx.store
		.publish_seller_version(SELLER, Timestamp::now(), &|_| Ok(()))
		.await
		.unwrap();
	let current = fx.store.current_seller_version(SELLER).await.unwrap().unwrap();
	assert_eq!(current.name, "Uj Nev Kft.");
	fx.with_pdf(&inv).await;
	let row = fx.row_of(&inv).await;
	fx.deliver(row.uid.as_str(), 1).await;
	let sends = fx.jobs("SEND_EMAIL").await;
	assert_eq!(sends[0].1["from_name"], "Teszt Kft.");
	assert_eq!(sends[0].1["vars"]["tenantName"], "Teszt Kft.");
}

#[tokio::test]
async fn an_unsent_invoice_and_its_storno_stay_quiet_until_a_resend() {
	let fx = fixture("optout").await;
	let d = fx.draft().await;
	let inv = fx.inv.issue_with(&fx.sys, d.uid.as_str(), Some(false)).await.unwrap();
	assert!(fx.rows(&inv).await.is_empty());
	assert!(fx.inv.mail_auto(&fx.sys, inv.uid.as_str()).await.unwrap().is_none());
	let st = fx.inv.storno(&fx.sys, inv.uid.as_str(), "t").await.unwrap();
	assert!(fx.rows(&st).await.is_empty(), "the storno follows its original");
	assert!(fx.inv.mail_auto(&fx.sys, st.uid.as_str()).await.unwrap().is_none());

	// A queued manual send clears the opt-out, so the storno now follows.
	let sent = fx.inv.mail_resend(&fx.sys, inv.uid.as_str()).await.unwrap();
	assert_eq!(sent.status, MailStatus::Queued);
	let row = fx.inv.mail_auto(&fx.sys, st.uid.as_str()).await.unwrap().unwrap();
	assert_eq!(row.kind, mintworks_invoice::store::MailKind::Storno);
}

#[tokio::test]
async fn no_address_is_a_no_email_row_and_a_problem_until_a_later_send() {
	let fx = fixture("no-email").await;
	fx.sql("UPDATE billing_parties SET email = NULL, emails = '[\"not an address\"]'")
		.await;
	let inv = fx.issued().await;
	let row = fx.row_of(&inv).await;
	assert_eq!((row.status, row.recipient.as_str()), (MailStatus::NoEmail, ""));
	assert!(fx.jobs(mail::KIND_MAIL).await.is_empty());
	assert_eq!(fx.problem_count().await, 1);

	// A manual send with no address answers NO_EMAIL and writes nothing.
	let sent = fx.inv.mail_resend(&fx.sys, inv.uid.as_str()).await.unwrap();
	assert_eq!((sent.status, sent.uid.as_str()), (MailStatus::NoEmail, ""));
	assert_eq!(fx.rows(&inv).await.len(), 1);

	fx.sql("UPDATE billing_parties SET emails = '[\"a@vevo.hu\"]'").await;
	let sent = fx.inv.mail_resend(&fx.sys, inv.uid.as_str()).await.unwrap();
	assert_eq!(fx.problem_count().await, 1, "a queued send does not resolve it yet");
	fx.sql(&format!("UPDATE invoice_emails SET status = 'SENT' WHERE uid = '{}'", sent.uid))
		.await;
	assert_eq!(fx.problem_count().await, 0);
}

#[tokio::test]
async fn a_failed_send_is_a_problem_only_inside_the_window() {
	let fx = fixture("problem-window").await;
	let inv = fx.issued().await;
	let row = fx.row_of(&inv).await;
	fx.store
		.mail_mark(row.uid.as_str(), MailStatus::Failed, Some(MailReason::SendFailed))
		.await
		.unwrap();
	assert_eq!(fx.problem_count().await, 1);
	let old = Timestamp::now().0 - 31 * 86_400;
	fx.sql(&format!("UPDATE invoice_emails SET at = {old}")).await;
	assert_eq!(fx.problem_count().await, 0);
	fx.set("invoice.mail.problem_days", "60").await;
	assert_eq!(fx.problem_count().await, 1);
}

#[tokio::test]
async fn a_resend_waits_out_the_gap_and_stops_at_the_maximum() {
	let fx = fixture("resend-limits").await;
	let inv = fx.issued_unsent().await;
	fx.inv.mail_resend(&fx.sys, inv.uid.as_str()).await.unwrap();
	let err = fx.inv.mail_resend(&fx.sys, inv.uid.as_str()).await.unwrap_err();
	assert_eq!(err.parts().1, "E-INV-RESEND-LIMIT");

	fx.set("invoice.mail.resend_gap_secs", "0").await;
	fx.set("invoice.mail.resend_max", "2").await;
	fx.inv.mail_resend(&fx.sys, inv.uid.as_str()).await.unwrap();
	let err = fx.inv.mail_resend(&fx.sys, inv.uid.as_str()).await.unwrap_err();
	assert_eq!(err.parts().1, "E-INV-RESEND-LIMIT");

	// A DAILY_LIMIT or NO_EMAIL row sent nothing, so it does not count.
	fx.sql("UPDATE invoice_emails SET status = 'FAILED', reason = 'DAILY_LIMIT'")
		.await;
	fx.inv.mail_resend(&fx.sys, inv.uid.as_str()).await.unwrap();
}

#[tokio::test]
async fn the_daily_cap_counts_recipients_fails_an_automatic_send_and_refuses_a_manual_one() {
	let fx = fixture("daily-cap").await;
	// Each mail is two recipients: `a@vevo.hu` plus `b@vevo.hu` in Cc.
	fx.set("invoice.mail.daily_max", "3").await;
	let (a, b) = (fx.issued_unsent().await, fx.issued_unsent().await);
	fx.inv.mail_auto(&fx.sys, a.uid.as_str()).await.unwrap();
	let over = fx.inv.mail_auto(&fx.sys, b.uid.as_str()).await.unwrap().unwrap();
	assert_eq!((over.status, over.reason), (MailStatus::Failed, Some(MailReason::DailyLimit)));
	assert_eq!(fx.jobs(mail::KIND_MAIL).await.len(), 1);
	let err = fx.inv.mail_resend(&fx.sys, b.uid.as_str()).await.unwrap_err();
	assert_eq!(err.parts().1, "E-INV-MAIL-DAILY-LIMIT");

	// The DAILY_LIMIT row does not count toward the cap.
	fx.set("invoice.mail.daily_max", "4").await;
	assert_eq!(
		fx.inv.mail_resend(&fx.sys, b.uid.as_str()).await.unwrap().status,
		MailStatus::Queued
	);
}

#[tokio::test]
async fn a_test_mode_invoice_has_the_smaller_cap() {
	let fx = fixture("daily-cap-test").await;
	fx.set("invoice.mail.daily_max_test", "0").await;
	let live = fx.issued().await;
	assert_eq!(fx.row_of(&live).await.status, MailStatus::Queued);
	let test = fx.issued_unsent().await;
	fx.sql(&format!("UPDATE invoices SET test_mode = 1 WHERE id = {}", test.id))
		.await;
	let row = fx.inv.mail_auto(&fx.sys, test.uid.as_str()).await.unwrap().unwrap();
	assert_eq!(row.reason, Some(MailReason::DailyLimit));
}

#[tokio::test]
async fn the_job_waits_for_the_pdf_and_then_gives_up() {
	let fx = fixture("pdf-wait").await;
	let inv = fx.issued().await;
	let row = fx.row_of(&inv).await;
	assert!(matches!(fx.deliver(row.uid.as_str(), 1).await, Next::Again { .. }));
	assert_eq!(fx.rows(&inv).await, [(MailStatus::Queued, None)]);
	assert!(matches!(fx.deliver(row.uid.as_str(), 20).await, Next::Done));
	assert_eq!(fx.rows(&inv).await, [(MailStatus::Failed, Some(MailReason::PdfTimeout))]);
	assert!(fx.jobs("SEND_EMAIL").await.is_empty());
}

#[tokio::test]
async fn the_job_hands_one_send_email_over_with_cc_and_language() {
	let fx = fixture("handoff").await;
	fx.inv
		.set_mail_intro(
			&fx.sys,
			&serde_json::json!({
				"invoice": { "hu": "Szia", "en": "Hi" }, "storno": { "hu": "", "en": "" },
			}),
		)
		.await
		.unwrap();
	fx.sql("UPDATE billing_parties SET doc_lang = NULL").await;
	let inv = fx.issued().await;
	fx.sql(&format!("UPDATE invoices SET doc_lang = 'hu-en', test_mode = 1 WHERE id = {}", inv.id))
		.await;
	fx.with_pdf(&inv).await;
	let row = fx.row_of(&inv).await;
	assert!(matches!(fx.deliver(row.uid.as_str(), 1).await, Next::Done));
	assert!(matches!(fx.deliver(row.uid.as_str(), 2).await, Next::Done));
	let sends = fx.jobs("SEND_EMAIL").await;
	assert_eq!(sends.len(), 1, "the keyed hand-off is at most once");
	let links: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM refs")
		.fetch_one(fx.store.write_pool())
		.await
		.unwrap();
	assert_eq!(links, 1, "a retry after the hand-off mints no second link");
	let (key, p) = &sends[0];
	assert_eq!(key, &format!("invmail-send:{}", row.uid.as_str()));
	assert_eq!(p["to"], "a@vevo.hu");
	assert_eq!(p["cc"], serde_json::json!(["b@vevo.hu"]));
	assert_eq!(p["template"], "invoice_mail");
	assert_eq!(p["lang"], "hu-en");
	assert_eq!(p["reply_to"], "info@teszt.hu");
	assert_eq!(p["from_name"], "Teszt Kft.");
	let vars = &p["vars"];
	assert_eq!(vars["subjectPrefix"], "[TESZT/TEST] ");
	assert_eq!(vars["intro"], "Szia\n\nHi");
	assert_eq!(vars["amount"], "1 270 Ft");
	assert_eq!(vars["sha256"], "f00d");
	assert!(vars["downloadUrl"].as_str().unwrap().starts_with("https://app.test/"));
	assert_eq!(fx.rows(&inv).await, [(MailStatus::Queued, None)], "settled later, not here");
}

#[tokio::test]
async fn a_storno_mail_names_its_original() {
	let fx = fixture("storno-mail").await;
	let inv = fx.issued().await;
	let st = fx.inv.storno(&fx.sys, inv.uid.as_str(), "t").await.unwrap();
	fx.with_pdf(&st).await;
	let row = fx.row_of(&st).await;
	fx.deliver(row.uid.as_str(), 1).await;
	let sends = fx.jobs("SEND_EMAIL").await;
	assert_eq!(sends[0].1["template"], "storno_mail");
	assert_eq!(sends[0].1["vars"]["originalNumber"], serde_json::json!(inv.number));
}

#[tokio::test]
async fn settle_maps_the_send_job_and_fails_a_stale_row_only_once_its_job_is_gone() {
	let fx = fixture("settle").await;
	let mut uids = Vec::new();
	for _ in 0..4 {
		let inv = fx.issued().await;
		fx.with_pdf(&inv).await;
		let row = fx.row_of(&inv).await;
		uids.push((inv, row.uid.as_str().to_owned()));
	}
	for (_, uid) in &uids[..3] {
		fx.deliver(uid, 1).await;
	}
	let key = |i: usize| format!("invmail-send:{}", uids[i].1);
	fx.sql(&format!("UPDATE jobs SET status = 'DONE' WHERE dedup_key = '{}'", key(0)))
		.await;
	fx.sql(&format!("UPDATE jobs SET status = 'FAILED' WHERE dedup_key = '{}'", key(1)))
		.await;
	// The fourth never reached a send job and was queued over an hour ago, but its own
	// `invmail:` job is still pending: `deliver` may yet hand it over.
	let old = Timestamp::now().0 - 3_601;
	fx.sql(&format!("UPDATE invoice_emails SET at = {old} WHERE uid = '{}'", uids[3].1))
		.await;
	let own = format!("invmail:{}", uids[3].1);
	fx.sql(&format!("UPDATE jobs SET run_at = {old} WHERE dedup_key = '{own}'"))
		.await;
	mail::settle(&fx.app, &fx.store).await.unwrap();
	assert_eq!(fx.rows(&uids[3].0).await, [(MailStatus::Queued, None)], "its job is live");
	fx.sql(&format!("UPDATE jobs SET status = 'FAILED' WHERE dedup_key = '{own}'"))
		.await;
	mail::settle(&fx.app, &fx.store).await.unwrap();
	assert_eq!(fx.rows(&uids[0].0).await, [(MailStatus::Sent, None)]);
	assert_eq!(fx.rows(&uids[1].0).await, [(MailStatus::Failed, Some(MailReason::SendFailed))]);
	assert_eq!(fx.rows(&uids[2].0).await, [(MailStatus::Queued, None)], "still pending");
	assert_eq!(fx.rows(&uids[3].0).await, [(MailStatus::Failed, Some(MailReason::SendFailed))]);
}

#[tokio::test]
async fn a_member_cannot_resend_and_a_draft_cannot_be_sent() {
	use mintworks_core::ctx::Actor;

	let fx = fixture("gates").await;
	fx.sql(&format!(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at) VALUES ({ORG}, 2, 'MEMBER', 0, 0)"
	))
	.await;
	let member = Ctx { actor: Actor::User { account_id: 2 }, ..Ctx::system("test").with_org(ORG) };
	let inv = fx.issued().await;
	let err = fx.inv.mail_resend(&member, inv.uid.as_str()).await.unwrap_err();
	assert_eq!(err.parts().0, axum::http::StatusCode::FORBIDDEN);
	// A member still reads the rows.
	fx.inv.mails(&member, inv.uid.as_str()).await.unwrap();
	let intro =
		serde_json::json!({ "invoice": { "hu": "", "en": "" }, "storno": { "hu": "", "en": "" } });
	assert_eq!(
		fx.inv.set_mail_intro(&member, &intro).await.unwrap_err().parts().0,
		axum::http::StatusCode::FORBIDDEN
	);

	let d = fx.draft().await;
	let err = fx.inv.mail_resend(&fx.sys, d.uid.as_str()).await.unwrap_err();
	assert_eq!(err.parts().1, "E-INV-NOT-ISSUED");
	let err = fx.inv.mail_auto(&fx.sys, d.uid.as_str()).await.unwrap_err();
	assert_eq!(err.parts().1, "E-INV-NOT-ISSUED");
}

#[tokio::test]
async fn the_intro_round_trips_and_refuses_a_link() {
	let fx = fixture("intro").await;
	let empty = fx.inv.mail_intro(&fx.sys).await.unwrap();
	assert_eq!(empty["invoice"]["hu"], "");
	let v = serde_json::json!({ "invoice": { "hu": "Szia", "en": "Hi" }, "storno": { "hu": "", "en": "" } });
	fx.inv.set_mail_intro(&fx.sys, &v).await.unwrap();
	assert_eq!(fx.inv.mail_intro(&fx.sys).await.unwrap(), v);
	let mut link = v.clone();
	link["storno"]["en"] = "https://evil.test".into();
	let err = fx.inv.set_mail_intro(&fx.sys, &link).await.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-SETTING");
	// The setting's own check guards every write path, Rune's `settings::set` included.
	let raw = link.to_string();
	let key = "invoice.mail.intro";
	let err =
		mintworks_core::settings::write_scoped(&fx.app, &fx.sys, key, Some(&raw), Some(ORG), None)
			.await
			.unwrap_err();
	assert_eq!(err.parts().1, "E-CORE-SETTING");
	assert_eq!(fx.inv.mail_intro(&fx.sys).await.unwrap(), v);
}

#[tokio::test]
async fn only_the_operator_writes_the_daily_caps() {
	use mintworks_core::ctx::Actor;
	use mintworks_core::settings::write_scoped;

	let fx = fixture("operator-only").await;
	fx.sql(&format!(
		"INSERT INTO memberships (org_id, account_id, role, accepted_at, created_at)
		 VALUES ({ORG}, 1, 'ADMIN', 0, 0), ({ROOT}, 2, 'ADMIN', 0, 0)"
	))
	.await;
	let user =
		|id| Ctx { actor: Actor::User { account_id: id }, ..Ctx::system("test").with_org(ORG) };
	let (admin, operator) = (user(1), user(2));
	let key = "invoice.mail.daily_max";
	let err = write_scoped(&fx.app, &admin, key, Some("100000"), Some(ORG), None)
		.await
		.unwrap_err();
	assert_eq!(err.parts().1, "E-AUTH-FORBIDDEN");
	write_scoped(&fx.app, &operator, key, Some("5"), Some(ORG), None).await.unwrap();
	assert_eq!(mail_int(&fx, key).await, 5);
	// The org Admin still writes an ordinary org-scoped key.
	write_scoped(&fx.app, &admin, "invoice.mail.resend_max", Some("3"), Some(ORG), None)
		.await
		.unwrap();
}

async fn mail_int(fx: &Fx, key: &str) -> i64 {
	match fx.app.settings.get_for(ORG, 0, key).await.unwrap() {
		mintworks_core::settings::SettingValue::Int(n) => n,
		v => panic!("{key}: {v:?}"),
	}
}

// vim: ts=4
