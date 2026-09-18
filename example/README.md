# Example application

A minimal, working SaaS built on this framework: a Rust binary that wires the framework
crates together and adds one consumer feature of its own, plus a React SPA that drives it.

It exists to prove the framework is usable **from outside** — nothing under `crates/` or
`adapters/` is touched by the example.

## What the demo does

A one-person consultancy books work against a customer, then bills it.

Two services are seeded into the catalogue:

| Code        | Unit       | Net price   | VAT       |
|-------------|------------|-------------|-----------|
| `CONSULT`   | hour       | 15 000 HUF  | 27% (std) |
| `SITEVISIT` | occasion   | 25 000 HUF  | 27% (std) |

Bookings are the example's own table and its own store trait — they are not a framework
concept. Checkout turns every unbilled booking into one draft invoice through the
framework's `Invoices` service; confirming it allocates a number and makes it immutable.

## The click-through, in order

1. **Register** (`/register`) — email, password, and the two consent checkboxes (Terms,
   Privacy). Registration is rate-limited by a proof-of-work challenge solved in a Web
   Worker; the "Verifying you're human" step is that.
2. **Check your email** (`/register/check-email`) — the activation mail is sent as a
   `SEND_EMAIL` job. **With no SMTP account configured the mail never arrives**; the screen
   says so and tells you to take the activation token from the backend log and open
   `/activate?token=…` by hand.
3. **Activate** (`/activate?token=…`) → **Log in** (`/login`).
4. **Book** (`/`) — pick a service, a date and a quantity, add an optional note. The list
   below shows what is booked and not yet billed.
5. **Checkout** — one click turns every unbilled booking into a `DRAFT` invoice and takes
   you to it. Nothing unbilled → nothing happens.
6. **Invoice detail** (`/invoices/:uid`) — lines, VAT per rate group, totals. From here:
   - **Confirm** issues the invoice: it gets a number from the `EX` series and becomes
     immutable.
   - **Record payment** marks it paid. **Payment is simulated** — there is no gateway
     until `saas-billing` exists — and it is **one-way**: a PAID invoice can never be
     stornoed, so Storno is gone afterwards. The dialog says so before it runs.
   - **Storno** cancels an issued invoice with a reason, producing a storno document. The
     bookings stay attached to the cancelled invoice; they are not returned to the unbilled
     pool.
   - **PDF** downloads the typst-rendered document once the `RENDER_PDF` job has run.
   - The **NAV** badge shows the reporting verdict read-only, or "NAV not configured".
7. **Billing** (`/billing`) — the billing party used as the invoice buyer.
8. **Account** (`/account`) — profile, consent records, GDPR export and account deletion.

Confirm, Record payment, Storno and Delete account are step-up routes: more than five
minutes after login they ask for your password again before they run.

## Running it

Every `cargo` command goes through `nix-shell` — outside it the build fails on libclang or
`libxml-2.0`.

The quickest path is `./start.sh` from the repo root: it starts mailpit as a mail sink, the
backend and esbuild in watch mode, and stops all three on Ctrl-C. It never sources `.env` —
that would export `MASTER_KEY` and the NAV keys into `pnpm install`, where any dependency's
lifecycle script could read them.

### Backend

```sh
cp example/backend/.env.example example/backend/.env   # then fill it in, see below
cd example/backend
nix-shell ../../shell.nix --run 'cargo run -p example-backend'
```

**Run it from `example/backend`.** `main.rs` loads `.env` from the crate directory whatever
the cwd, but `DB_PATH=./data/example.db` and `DIST_DIR=../frontend/dist` are relative to the
process, and from the repo root they resolve to a *different* database and to a path outside
the repo. `example/backend/data/` is the one `example/.gitignore` covers.

It listens on `127.0.0.1:8080`, migrates both schema modules — the framework's `saas` and this
application's own `example`, which owns the `bookings` table — and seeds the seller, the two
services and the legal documents on every start (seeding is idempotent).

#### Upgrading from the step ledger

A database created before 2026-09-17 carries a `migrations` table of named, checksummed steps
instead of `schema_version`. The framework's seven steps plus `example/bookings` are exactly
framework v1 + example v1, so such a database is stamped by hand rather than recreated —
**never** delete it, its `doc_series` holds NAV invoice numbers already filed, and a fresh
database restarts at `EX2026/000001`, which NAV rejects as `INVOICE_NUMBER_NOT_UNIQUE`.

With the app stopped and the file backed up (`cp example.db example.db.bak`, plus any
`-wal`/`-shm`):

```sh
sqlite3 example/backend/data/example.db <<'SQL'
PRAGMA foreign_keys=OFF;
BEGIN;
DROP TABLE migrations;
CREATE TABLE schema_version (
	module     TEXT    NOT NULL PRIMARY KEY,
	version    INTEGER NOT NULL,
	updated_at INTEGER NOT NULL
);
INSERT INTO schema_version (module, version, updated_at)
	VALUES ('saas', 1, unixepoch()), ('example', 1, unixepoch());
DELETE FROM vars WHERE name = 'db_version';
COMMIT;
SQL
```

Check first that the database really is at that shape — `SELECT name FROM migrations` should
list exactly `saas-core/init`, `saas-auth/init`, `saas-invoice/init`, `saas-nav/init`,
`saas-core/drop-job-max-attempts`, `saas-core/job-claimed-at`, `saas-core/job-claim-index` and
`example/bookings`, and nothing else. The stamp is version 1; the next boot applies every
`if from < N` block the build ships and reports the resulting version at `/readyz` (2 as of
this writing — it tracks `schema::VERSION`).

### Frontend

The backend serves the built SPA itself from `DIST_DIR` (default `../frontend/dist`) as the
router's fallback, so there is one origin and `BASE_URL` is the backend's own:

```sh
cd example/frontend && pnpm install && pnpm build
cd ../backend
nix-shell ../../shell.nix --run 'cargo run -p example-backend'   # http://localhost:8080
```

`pnpm watch` rebuilds `dist/` on every edit; a reload picks the bundle up, with no second
server in front of the backend.

Other scripts: `pnpm typecheck`, `pnpm check` (Biome lint + format check), `pnpm format`.

## Environment

Values live only in `example/backend/.env`, which is gitignored. `.env.example` lists the
names. **No credential is ever written into a tracked file.**

| Variable | Purpose |
|----------|---------|
| `MASTER_KEY` | 32 bytes base64. Required, never auto-generated — it decrypts the `secrets` table. |
| `DB_PATH` | SQLite file. |
| `DATA_DIR` | Generated PDFs and other blobs. |
| `LISTEN` | Bind address, e.g. `127.0.0.1:8080`. |
| `BASE_URL` | Origin used in mailed links — the backend's own. |
| `DIST_DIR` | Built SPA to serve; defaults to `../frontend/dist`. |
| `SAAS_EMAIL_FROM` | Sender address. |
| `SAAS_EMAIL_SMTP_HOST` | SMTP host. |
| `SAAS_EMAIL_SMTP_PORT` | SMTP port. |
| `SAAS_EMAIL_SMTP_USERNAME` | SMTP username. |
| `SAAS_EMAIL_SMTP_TLS_MODE` | TLS mode. |
| `SELLER_TAX_NUMBER` | The seller's Hungarian tax number: 11 digits, written `12345678-2-42` or `12345678242`. Punctuation is stripped, a wrong length refuses boot. |
| `SELLER_NAME` | The seller's registered name. |
| `SELLER_POSTCODE` | |
| `SELLER_CITY` | |
| `SELLER_STREET` | The seller's address. All four, with the tax number, are **required** — the backend refuses to boot without them, because a placeholder would issue numbered, immutable invoices under somebody else's identity and land in the NAV `supplierAddress`, where it cannot be corrected. |
| `SELLER_BANK_ACCOUNT` | Optional. An invoice without a bank account is legal, one with a wrong one is not. |
| `SELLER_BANK_NAME` | Optional, beside the account. |
| `SELLER_EU_VAT_ID` | Optional, and **not** derived from the tax number: a company has one only once it registers for intra-Community trade. Validated by `saas_invoice::vies::normalise`, and sent to VIES as the requester on a cross-border check. |
| `SMTP_PASSWORD` | Seeded once into the encrypted `secrets` table as `smtp.password`, then never read back out over HTTP. |

The `SELLER_*` values seed the seller's first published version, and only on the **first** boot.
After that the database is the source of truth: edit the seller through
`PATCH /api/seller/draft` and make it live with `POST /api/seller/publish` (operator only).
Re-publishing the environment on every restart would either undo that or stack up an identical
version per boot. Invoices freeze the version they were issued under, so a published edit never
changes a PDF or a NAV filing that already exists — `GET /api/seller/history` shows which
version was in force when.

Everything else lives in the database `settings` and `secrets` tables. `seed.rs` bootstraps
the NAV secrets from the environment on every start — the names are in `.env.example`, and
the section below says which land where.

### NAV sandbox credentials

NAV reporting is optional and the example only surfaces it read-only. With nothing
configured, every invoice shows "NAV not configured" and no `NAV_REPORT` job is filed.

To wire up the NAV Online Számla 3.0 test system you need, in the database:

- on the seller row (`sellers.id = 1`, written by `seed.rs`): `nav_login`, and `nav_base_url`
  pointed at the sandbox — left blank it falls back to the `nav.base_url` setting;
- in the encrypted `secrets` table: `nav.tech_password`, `nav.sign_key`, `nav.exchange_key`;
- in `settings`: `nav.base_url`, pointed at the sandbox.

The `nav.software_*` block needs nothing from you. It identifies *this program*, which is the
same in every deployment of it, so it is compiled in — `NAV_SOFTWARE` in `src/seed.rs` — and
re-seeded on every boot. Four of those keys are `.required()`, and a blank one used to take
boot down before the first invoice was ever issued.

A setting also resolves from the environment — the variable is `SAAS_` plus the key
uppercased with `.` as `_`, so `nav.base_url` is `SAAS_NAV_BASE_URL`, which is the one NAV
setting that legitimately differs per deployment. Secrets do not: they are write-only over
HTTP and are never read back out, so they go into the table directly. The example exposes no
endpoint that reads or writes settings.

### A consumer's own tables are outside GDPR export and erasure

`saas-auth`'s GDPR export and erasure run off a **closed** column allowlist
(`crates/saas-auth/src/gdpr.rs`; `ERASURE` is `pub(crate)` so no caller and no store adapter
can widen it), and erasure is anonymisation rather than deletion. Neither reaches a consumer
table, so this example's `bookings` — the free-text `note` column included — is neither
exported nor anonymised by the framework.

That is a seam, not a bug: a consumer that stores personal data in its own tables has to say
so in its privacy policy and handle the export and erasure of it itself. `legal/privacy.md`
here says exactly what the framework does and does not do.

## What is not real here

- **Payment is simulated.** "Record payment" only sets the invoice's paid state. There is
  no gateway, no redirect and no webhook until `saas-billing` and a payment adapter exist.
  It is one-way — `mark_status` requires `ISSUED`, so **Storno is unavailable once paid** —
  and the framework deliberately mounts no HTTP route for `mark_paid`: marking an invoice
  paid is a payment provider's report, not the payer's claim. The example exposes it to the
  tenant user anyway because it seeds no operator account; a real consumer must not.
- **Confirm and Cancel mint legal documents on the buyer's say-so.** Both run against
  `sellers.id = 1` — the operator's own taxpayer id, from `SELLER_TAX_NUMBER` — so a tenant
  user issues and stornos numbered invoices under it, each burning a number in the `EX` series
  and queueing a NAV filing. Step-up and the per-account rate tier bound the rate, and
  `MAX_QTY_E6` (24 units per booking) bounds the face value; a real consumer gates both routes
  on an operator role or on a payment the provider reported.
- **"Record payment" confirms the amount.** The request carries the gross the client believes
  it is paying and a mismatch is refused, because the transition cannot be undone.
- **Email needs your own SMTP account.** Without one, activation and password-reset mails
  fail and you work from the token in the log.
- **One seller, one series, two services**, all seeded. There is no admin UI — the seller's
  draft/publish routes are the one piece of operator master-data editing the example exposes,
  and it seeds no operator account, so you reach them with a hand-made operator token.
