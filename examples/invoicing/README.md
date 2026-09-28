# `examples/invoicing` — an invoicing application, written in Rune

The application is the `script/` directory beside this file: **no Rust file in it**, the
sources `saas-run` compiles at startup, plus the `frontend/` back-office SPA it serves. It is a
**multi-tenant invoicing SaaS**: each registered business onboards itself with its own seller,
tax number and NAV credentials. It mounts the framework's read, party, service, seller and NAV
connection route bundles, wraps the whole draft lifecycle — draft, patch, lines, issue, storno —
in its own handlers, and declares its own `project` object type. Nothing is seeded at boot.

It is **not a Cargo crate** — it is not a workspace member, so `cargo test --all` and the
pre-commit hook do not cover it. Its suite runs through `saas-run test`.

## Running it

```sh
nix-shell --run 'cargo build -p saas-run'          # from the repository root
./target/debug/saas-run examples/invoicing/script             # serves on http://localhost:8081
```

Paths in `.env` are relative to the **process**, not to this directory, so run from the
repository root. The database lands in `./data/script-app.db`, separate from
`examples/booking/backend`'s: they are two applications over two files and must never contend.

`script/.env` is gitignored — `saas-run` loads `<app-dir>/.env`, so it lives inside the
application directory and not beside this README. A fresh checkout needs one holding
`MASTER_KEY` (32 bytes base64, never auto-generated), `DB_PATH`, `DATA_DIR`, `LISTEN`,
`BASE_URL`, `DIST_DIR`, the two required mail settings
`EMAIL_FROM` and `EMAIL_SMTP_HOST`, `DEPLOYMENT_ENV` (`test` or `production` — which NAV system
tenants file with). The `nav.software_*` identity of this invoicing programme, which the `nav`
feature requires, is compiled into `script/main.rn` for `DEPLOYMENT_ENV=test` only; for production,
fill in the `NAV_SOFTWARE_PROD` template there or set `NAV_SOFTWARE_*` to the identity NAV
registered. The global
`NAV_TECH_PASSWORD`/`NAV_SIGN_KEY`/`NAV_EXCHANGE_KEY` only matter for a root-org seller; tenants
store their own. The copy in `script/` documents each one.

**Upgrading from the seeded version:** the `APP_SELLER_*` values and `APP_NAV_LOGIN` are no
longer read — delete them from your `script/.env`. `seed.rn` is gone. An existing database keeps
its root-org seller row (deleting it would burn invoice numbers already reported to NAV); it is
simply no longer what new users invoice under. A production deployment that relied on the
compiled-in `nav.software_*` identity now refuses to boot until `NAV_SOFTWARE_*` is set.

## Onboarding

1. Register and activate. The account lands in its PERSONAL org, which cannot invoice, so the
   SPA sends it to `/setup`.
2. **Company details** (required): the SPA creates a SHARED org (`POST /api/orgs`), switches to
   it (`POST /api/auth/switch-org`) and mints its seller (`POST /api/seller`). A mint that fails
   after the org exists resumes on the next submit.
3. **NAV connection** (skippable): `PUT /api/nav/credentials` is step-up gated and verified with
   one NAV `tokenExchange` before anything is stored.
4. Issuing works before NAV is connected. Each `NAV_REPORT` defers quietly and a warning band
   stays on every page. Connecting files the backlog at once.

The Company settings page (`/settings/company`) edits the details (`PUT /api/seller`, a new
version only when something changed) and changes the NAV credentials.

## The frontend

`frontend/` is a React back-office SPA — dashboard, invoice workspace, partners, services,
projects and full auth parity, in en and hu, with its own tokens and dark mode. `saas-run`
serves it itself as the router's fallback, so there is one origin and `BASE_URL` is the
application's own.

```sh
pnpm install                                     # from the repository root: one pnpm workspace
pnpm --filter saas-invoicing-frontend build      # writes frontend/dist/
```

`script/.env` carries `DIST_DIR=../frontend/dist`. That one value is **app-dir** relative, not
process relative (`bin/saas-run/src/app.rs::dist_dir` joins it onto the application directory),
and a configured path that is not a directory is a startup failure — so build `frontend/dist/`
before the first run, or comment the line out.

`pnpm --filter saas-invoicing-frontend watch` rebuilds on every edit. Other scripts:
`typecheck`, `lint`, `format`. None of them is covered by `cargo test --all` or by the
pre-commit hook.

The generic half — transport, auth flows, wire types, money formatting, the `E-*` dictionaries —
is `@saas-framework/client` (`js/saas-client`), shared with `examples/booking/frontend`. What
lives here is this application's own markup.

**The invoice-level discount is set when the draft is created and cannot be changed afterwards.**
`saas_invoice::store::InvoicePatch` carries no `discount_kind` and never reads `discount_value`
off the wire, so the composer offers it on creation only.

## The tests

```sh
./target/debug/saas-run test examples/invoicing/script                    # every case
./target/debug/saas-run test examples/invoicing/script cross_org          # one, by name substring
```

Each case gets its own temporary file database and its own full build, so no case sees
another's rows; a failing case exits 1.

## What is where

| file | |
|---|---|
| `main.rn` | the whole served surface — mounts and routes — plus the shared helpers |
| `invoices.rn` | the draft lifecycle, the summary, and one `tx::with` block that drafts and issues together |
| `projects.rn` | the script-declared `project` type and the `invoice.ext` link to it |
| `tests.rn` | `#[test] pub async fn` cases: onboarding, the mounted CRUD surfaces, the full cycle, the project link, the org boundary |
| `../frontend/` | the back-office SPA, on `@saas-framework/client` |

Services and billing parties have no source of their own: `invoice.org_services` and
`invoice.org_parties` serve `/api/services` and `/api/billing-parties` already, both keyed by
uid, and a wrapper over them would only be a second spelling of the same rows.

## Projects — the object store, without a migration

`main.rn` declares two object types, and that declaration is the whole schema:

```rune
app.object_type("project",     #{ prefix: "prj", paths: ["$.partyUid"] });
app.object_type("invoice.ext", #{ paths: ["$.projectUid"] });
```

The declared paths are reconciled at startup — a script author writes no migration and bumps no
version — and `objects::query` refuses a path that was not declared rather than answering with an
empty page.

An invoice is linked to a project by an `invoice.ext` row keyed by the invoice's **own uid**, not
by a field on the invoice. Ext data is a side table, so the link is still writable once the
invoice is `ISSUED` and immutable. Listing a project's invoices is one index query for the
membership plus one `invoices::invoice` fetch per uid: there is no bulk fetch-by-uid, so a page of
50 costs 51 round trips against the store.
