//! Invoicing: money and VAT arithmetic, tax rules, numbering and the invoice lifecycle.
//!
//! Every amount is an integer minor unit and every rate an integer basis point. No float
//! appears in the money path — not in computation, not in serialization, not in a fixture.

#![forbid(unsafe_code)]

// `party`'s `normalise_country` is re-exported below — it is one of the rules
// `saas_nav::auth::check_seller` applies to the `sellers` row, so the seller and buyer sides
// cannot drift.
pub(crate) mod catalog;
pub mod currency;
pub mod draft;
pub mod issue;
pub mod mnb;
pub mod money;
pub mod numbering;
pub(crate) mod party;
pub mod pdf;
pub mod pricing;
pub mod routes;
pub mod service_api;
pub mod store;
pub mod storno;
pub mod taxrule;
pub mod vat;
pub mod vies;

pub use currency::{Currency, RateMode, check_currency_settings, to_base};
pub use draft::{IssueNow, Line, NewDraft, Party};
// `saas-nav` files the invoice this crate issues, so the job kind it handles, the one seller
// and the store accessor are part of the interface rather than of `service_api`'s innards.
pub use issue::{KIND_NAV_REPORT, invoice_job_payload, tax_digits, vat_code_ok};
pub use money::{Discount, DraftLine, apportion, discount_of, discount_parts};
pub use numbering::date_of;
pub use party::normalise_country;
pub use pdf::{TEMPLATE_VERSION, render};
pub use pricing::PricingHook;
pub use routes::{operator, tenant_invoices, tenant_parties, tenant_read};
pub use service_api::{FullInvoice, Invoices, LinePatch, SELLER_ID, store as invoice_store};
pub use store::{
	DiscountKind, Invoice, InvoiceKind, InvoiceLine, InvoiceStore, InvoiceVatGroup, PartyKind,
	PaymentMethod, Seller, Service, render_number,
};
pub use taxrule::{BuyerProfile, BuyerZone, Verdict, determine};
pub use vat::{ComputedInvoice, ComputedLine, VatClass, VatCode, VatGroup, compute};
pub use vies::ViesResult;

// vim: ts=4
