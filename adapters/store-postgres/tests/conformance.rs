//! `PgStore` under the shared conformance suite (`mintworks_store_conformance`), one module per
//! store trait the adapter implements. Skips every test when `PG_TEST_URL` is unset.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

mod tx {
	use super::common::PgHarness;
	mintworks_store_conformance::tx_tests!(PgHarness);
}

mod job_claim {
	use super::common::PgHarness;
	mintworks_store_conformance::job_claim_tests!(PgHarness);
}

mod auth {
	use super::common::PgHarness;
	mintworks_store_conformance::auth_tests!(PgHarness);
}

mod auth_ext {
	use super::common::PgHarness;
	mintworks_store_conformance::auth_ext_tests!(PgHarness);
}

mod invoice {
	use super::common::PgHarness;
	mintworks_store_conformance::invoice_tests!(PgHarness);
}

mod billing {
	use super::common::PgHarness;
	mintworks_store_conformance::billing_tests!(PgHarness);
}

mod objects {
	use super::common::PgHarness;
	mintworks_store_conformance::objects_tests!(PgHarness);
}

mod pdf {
	use super::common::PgHarness;
	mintworks_store_conformance::pdf_tests!(PgHarness);
}

mod plans {
	use super::common::PgHarness;
	mintworks_store_conformance::plans_tests!(PgHarness);
}

mod refs {
	use super::common::PgHarness;
	mintworks_store_conformance::refs_tests!(PgHarness);
}

mod entitle {
	use super::common::PgHarness;
	mintworks_store_conformance::entitle_tests!(PgHarness);
}

#[cfg(feature = "ai")]
mod agent {
	use super::common::PgHarness;
	mintworks_store_conformance::agent_tests!(PgHarness);
}

#[cfg(feature = "ai")]
mod llm {
	use super::common::PgHarness;
	mintworks_store_conformance::llm_tests!(PgHarness);
}

#[cfg(feature = "ai")]
mod search {
	use super::common::PgHarness;
	mintworks_store_conformance::search_tests!(PgHarness);
}

// vim: ts=4
