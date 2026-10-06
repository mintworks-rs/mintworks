# Symbion Mintworks

*business apps, freshly minted.*

**An embeddable back office for Rust and Rune applications.**

Mintworks is a workspace of Rust library crates that an application embeds to get accounts and
tenants, invoicing with Hungarian NAV Online Számla reporting, payments, entitlements and
subscriptions, email, PDF rendering, background jobs and, optionally, an LLM agent harness. An
application is written either in Rust against the service handles, or in Rune and served by the
`mintworks` binary with no Rust file of its own. It is **not** an ERP product, not a hosted
service and not a web framework: it is the back office you build one on.

## Crates

| Package | What it is |
|---|---|
| `mintworks-core` | Shared foundation: errors and wire envelope, value types, ids, money, config and secrets, settings, jobs, rate limiting, audit log |
| `mintworks-auth` | Accounts, tenants, memberships, API keys, TOTP and consent |
| `mintworks-email` | Handlebars email templates and retrying SMTP delivery |
| `mintworks-invoice` | Invoicing: fixed-point money, VAT groups, numbering and PDFs |
| `mintworks-pdf` | Typst document rendering: a sandboxed World over a template set |
| `mintworks-nav` | NAV Online Számla 3.0 reporting and the statutory audit data export |
| `mintworks-billing` | Gateway-agnostic payments: the `PaymentProvider` trait, settlement and allocation |
| `mintworks-entitle` | Entitlements: declared features, limits and meters over grants and a usage ledger |
| `mintworks-plans` | Offers, purchases, rewards, coupons and subscriptions; sells what `mintworks-entitle` grants |
| `mintworks-llm` | OpenAI-compatible streaming LLM client with role profiles, fallback and a cost ledger (`ai`) |
| `mintworks-memory` | Versioned markdown memory with full-text search (`ai`) |
| `mintworks-agent` | Agent harness: threads, runs, the tool loop and its event stream (`ai`) |
| `mintworks-search` | Web search and page fetching with a shared, cited source store (`ai`) |
| `mintworks-script` | Rune scripting over the service handles |
| `mintworks-store-conformance` | The conformance suite every store adapter runs (dev-only) |
| `mintworks` | The binary: serves a complete application from a directory of Rune sources |
| `@mintworks/client` | The headless browser client (`js/client`): transport, auth flows, wire types |

## Adapters

The only engine- and vendor-aware code, under `adapters/`. Nothing in the crates above depends
on them; the application or the `mintworks` binary constructs them.

| Package | What it is |
|---|---|
| `mintworks-store-sqlite` | SQLite store adapter: every store trait, the DDL and the migration runner |
| `mintworks-store-postgres` | PostgreSQL store adapter (`postgres`) |
| `mintworks-appdb-sqlite` | SQLite database for a Rune application's own tables, separate from the framework store |
| `mintworks-appdb-postgres` | PostgreSQL database for a Rune application's own tables (`postgres`) |
| `mintworks-payment-barion` | Barion Smart Gateway `PaymentProvider` |
| `mintworks-search-linkup` | Linkup web search `SearchProvider` |
| `mintworks-search-searxng` | SearXNG web search `SearchProvider` over a self-hosted instance |
| `mintworks-fetch-jina` | Jina Reader page fetch and Jina Search |

`ai` and `postgres` are Cargo features of the `mintworks` binary, off by default.

`examples/` holds complete applications: a booking app twice (`booking/app-rust` in Rust,
`booking/app-rune` in Rune), an invoicing back office, a subscription shop, a research
assistant and an agent demo, each with its SPA.

## Quickstart

The build needs `libxml2`, `pkg-config` and libclang; `shell.nix` provides them, so run cargo
through it:

```sh
nix-shell --run 'cargo build -p mintworks'
```

The booking example needs a few deployment values. Put them in
`examples/booking/app-rune/.env` (git-ignored):

```sh
DEPLOYMENT_ENV=test
LISTEN=127.0.0.1:8082
BASE_URL=http://localhost:8082
APP_BASE_URL=http://localhost:8082
DB_PATH=./data/quickstart.db
EMAIL_FROM=noreply@example.com
EMAIL_SMTP_HOST=localhost
APP_SELLER_NAME="Example Kft."
APP_SELLER_TAX_NUMBER=12345678-2-42
APP_SELLER_POSTCODE=1011
APP_SELLER_CITY=Budapest
APP_SELLER_STREET="Fő utca 1."
APP_SELLER_BANK_ACCOUNT=11111111-22222222-33333333
APP_SELLER_BANK_NAME="Example Bank"
APP_SELLER_SERIES_CODE=EX
```

Then, from the repository root:

```sh
MASTER_KEY=$(openssl rand -base64 32) ./target/nix-shell/debug/mintworks examples/booking/app-rune
```

The log prints a one-time link to register the first operator. Without the NAV keys invoices are
issued but not filed, and without SMTP no email leaves. Keep `MASTER_KEY` once you have data: it
encrypts the stored secrets. The SPA is built separately (`examples/booking/frontend`, served
when `DIST_DIR=../frontend/dist` is set); `examples/booking/app-rune/README.md` has the rest.

A Rune application's tests run with `mintworks test <app-dir> [name-filter]`.

## Scope

- NAV Online Számla **3.0** (Hungary).
- SQLite and PostgreSQL, for both the framework database and the application database.
- Minimum supported Rust version: **1.94** (`rust-version` in `Cargo.toml`).
- Not built yet: `mintworks-admin` and `server/`.
- Version 0.5.0: pre-1.0, so a minor release may break the API.

## Disclaimer

Mintworks is provided **without warranty of any kind**, as the licence states. It is **not tax or
legal advice**. The operator running it is responsible for their invoices, their NAV
submissions and their compliance with the law that applies to them.

## License

- The framework: [MPL-2.0](LICENSE).
- `examples/`: [MIT-0](examples/LICENSE).
- The vendored NAV schemas in `crates/nav/xsd/`: MIT, © Nemzeti Adó- és Vámhivatal
  ([`LICENCE.md`](crates/nav/xsd/LICENCE.md)).
- The fonts embedded into every binary through `typst-assets`: their own licences, reproduced in
  [`licenses/typst-assets-NOTICE`](licenses/typst-assets-NOTICE).

See [CONTRIBUTING.md](CONTRIBUTING.md) to contribute and [SECURITY.md](SECURITY.md) to report a
vulnerability.
