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
concept. Checkout turns every unbilled booking into one invoice through the framework's
`Invoices` service, and asks how you want to pay. **The customer is never asked for a
password to raise an invoice**: issuing is a consequence of their own payment-method choice
and runs as `Actor::System`, which the step-up gate exempts.

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
5. **Checkout** — you pick how to pay, and that decides what the invoice is:
   - **Bank transfer** issues it there and then, with `paymentMethod = TRANSFER`, a number
     from the `EX` series, a due date and a downloadable PDF. A transfer needs a number to
     quote as its reference, so there is nothing to wait for.
   - **Pay by card** drafts it, opens a payment at the gateway and sends you there. The
     invoice is issued — and stamped `CARD` — when the money actually lands.
   - **Pay by card appears only when a gateway is registered.** With no `PAYMENT_BARION_POS_KEY` in
     `.env` the offline demo offers bank transfer alone.
   - Nothing unbilled → nothing happens.
6. **Invoice detail** (`/invoices/:uid`) — lines, VAT per rate group, totals, and a **Payment**
   panel that is the whole of the payment flow after checkout:
   - A payment in progress offers **Continue payment**, which sends you back to the gateway
     with the URL the payment was opened with. Pressing the browser's back button out of the
     gateway and returning here is exactly what that is for.
   - A failed, cancelled or expired attempt offers **Pay again** (a fresh payment under a
     fresh key) and **Pay another way**.
   - **Pay another way** gives up on the open card payment, restamps the draft as a bank
     transfer and issues it — the escape hatch for a card attempt you have walked away from.
     It cancels that payment locally because a gateway keeps reporting an abandoned one as
     live until it expires, and a card payment freezes the invoice (`PENDING`) while it is.
   - **Discard draft** throws an unpaid draft away and returns its bookings to the unbilled
     list, ready to be checked out again.
   - **PDF** downloads the typst-rendered document once the `RENDER_PDF` job has run.
   - The **NAV** badge shows the reporting verdict read-only, or "NAV not configured".
   - **Storno is not here.** Cancelling a numbered document is an operator's job, not the
     payer's.
7. **Billing** (`/billing`) — the billing party used as the invoice buyer.
8. **Account** (`/account`) — profile, consent records, GDPR export and account deletion.

**A transfer invoice stays unpaid in this demo.** An invoice reaches `PAID` only when a
gateway's webhook reports the money or an operator records it by hand through
`POST /api/admin/payments`, which is operator-only — and the example seeds no operator
account. Settling one means minting an operator token yourself.

Refund and Delete account are the step-up routes left: more than five minutes after login
they ask for your password again before they run.

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
nix-shell ../../shell.nix --run 'cargo run -p saas-example'
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
nix-shell ../../shell.nix --run 'cargo run -p saas-example'   # http://localhost:8080
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
| `JOBS_WORKERS` | Job workers this process runs, overriding the `jobs.workers` setting. Per-process, so a second replica sharing the database sets it to `0` and runs no runner. Normally unset. |
| `DIST_DIR` | Built SPA to serve; defaults to `../frontend/dist`. The `dist_dir` setting declares it, but the router is assembled before any `App` exists, so it is read from here. |
| `EMAIL_FROM` | Sender address. |
| `EMAIL_SMTP_HOST` | SMTP host. |
| `EMAIL_SMTP_PORT` | SMTP port. |
| `EMAIL_SMTP_USERNAME` | SMTP username. |
| `EMAIL_SMTP_TLS_MODE` | TLS mode. |
| `DEPLOYMENT_ENV` | Which environment **both** NAV and the payment gateway run against: `test` / `production`. Defaults to **production**, so this is the one line that keeps the demo out of the statutory NAV system *and* out of live Barion. One flag, not one per system: a deployment filing test invoices while taking real card money is a failure mode, not a configuration. |
| `SELLER_TAX_NUMBER` | The seller's Hungarian tax number: 11 digits, written `12345678-2-42` or `12345678242`. Punctuation is stripped, a wrong length refuses boot. |
| `SELLER_NAME` | The seller's registered name. |
| `SELLER_POSTCODE` | |
| `SELLER_CITY` | |
| `SELLER_STREET` | The seller's address. All four, with the tax number, are **required** — the backend refuses to boot without them, because a placeholder would issue numbered, immutable invoices under somebody else's identity and land in the NAV `supplierAddress`, where it cannot be corrected. |
| `SELLER_BANK_ACCOUNT` | Optional. An invoice without a bank account is legal, one with a wrong one is not. |
| `SELLER_BANK_NAME` | Optional, beside the account. |
| `SELLER_EU_VAT_ID` | Optional, and **not** derived from the tax number: a company has one only once it registers for intra-Community trade. Validated by `saas_invoice::vies::normalise`, and sent to VIES as the requester on a cross-border check. |
| `EMAIL_SMTP_PASSWORD` | The `email.smtp.password` secret, resolved from here on every boot and never stored. |

The `SELLER_*` values seed the seller's first published version, and only on the **first** boot.
After that the database is the source of truth: edit the seller through the `Invoices` handle's
`save_seller_draft` and `publish_seller` methods. The five HTTP routes that used to expose them
are gone; seller version administration is rebuilt in `saas-admin` under plan
`saas-7-admin-example`. Re-publishing the environment on every restart would either undo that
or stack up an identical version per boot. Invoices freeze the version they were issued under,
so a published edit never changes a PDF or a NAV filing that already exists —
`Invoices::seller_history` shows which version was in force when.

Everything else lives in the database `settings` and `secrets` tables, or in the environment as
a fallback for either — the names are in `.env.example`, and the section below says which land
where. One rule for the whole file: an unprefixed `SCREAMING_SNAKE` name is a framework key,
the declared key uppercased with `.` and `-` replaced by `_`; this application's own keys are
unprefixed too, and `saas-core` declares none of them. A blank value means *absent*.

### NAV sandbox credentials

NAV reporting is optional and the example only surfaces it read-only. With nothing
configured, every invoice shows "NAV not configured" and no `NAV_REPORT` job is filed.

To wire up the NAV Online Számla 3.0 test system you need, in the database:

- on the seller row (`sellers.id = 1`, written by `seed.rs`): `nav_login`, and `nav_base_url`
  pointed at the sandbox — left blank it falls back to the settings below;
- as secrets: `nav.tech_password`, `nav.sign_key`, `nav.exchange_key` — in the encrypted
  `secrets` table, or in the environment as `NAV_TECH_PASSWORD` and friends, which is what
  `.env.example` uses;
- in `settings`: `deployment.env` = `test`. `nav.base_url` is the explicit-endpoint override,
  for a host `deployment.env` cannot name; blank, which is its default, means "derive it from
  `deployment.env`".

The `nav.software_*` block needs nothing from you. It identifies *this program*, which is the
same in every deployment of it, so it is compiled in — the `setting_default` calls in
`src/main.rs`. Six of those keys are `.required()`, and a blank one takes boot down. A registered
default sits **below** the environment, so `NAV_SOFTWARE_ID` in `.env` still overrides it; the
`settings` rows this used to seed sat above everything and could never be overridden.

Settings **and secrets** resolve from the environment — the variable is the key uppercased with
`.` and `-` as `_`, unprefixed, so `deployment.env` is `DEPLOYMENT_ENV`, which is the one
setting every deployment has to get right, and `email.smtp.password` is `EMAIL_SMTP_PASSWORD`.
A setting resolves row, then environment, then the application's registered default, then the
registry default. Nothing is seeded into the database: a seeded row would shadow the variable on
every later boot, so editing `.env` and restarting would silently do nothing. For a secret the
environment goes further and **beats** the row, so rotating one is a redeploy and a value in
`.env` is never written to `secrets` — the trade-off being that it is then visible in
`docker inspect`. The example exposes no endpoint that reads or writes settings.

### Barion sandbox credentials

Payment is optional the same way. With nothing configured the app boots with no gateway
registered, the SPA offers no card button, and the only route to PAID is bank transfer plus an
operator's manual entry.

Which Barion environment is `DEPLOYMENT_ENV` — the same flag NAV reads, so the gateway
cannot end up in the sandbox while invoices are filed statutorily. `PAYMENT_BARION_PAYEE`
and `PAYMENT_BARION_POS_KEY` in `.env` are the credentials, and `BarionProvider` resolves
them on every call — the payee as the `payment.barion.payee` setting, the POS key as the
`payment.barion.pos_key` secret. Neither is written to the database, so rotating the POS key is a
redeploy; the accepted cost is that a live payment credential is then visible in
`docker inspect` and `/proc/<pid>/environ`.

**`PAYMENT_BARION_POS_KEY` also has to be set for the gateway to exist at all**: `main.rs`
registers the provider only when that variable is set, and an unregistered provider means no
card button. The provider map is an `AppBuilder::extension` and `build` freezes those before an
`App` exists, so no credentials-resolving check is available at composition time — the env var
is the honest approximation of "a gateway is configured". The provider itself is constructed
unresolved and handed the `App` from `on_init`, where the credentials become resolvable.

Barion cannot POST to `localhost`, so on a developer machine the IPN callback never arrives.
The demo settles anyway: reading an invoice's payments re-asks `GetPaymentState`, which is what
Barion prescribes for the return leg — "the callback signal is only a signal", and the state is
to be fetched when the payer is redirected back. A deployment that wants the callback proper
needs a publicly reachable `BASE_URL` (a tunnel in development).

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

- **Payment is real, but only with an account.** `saas-billing` and `payment-adapter-barion`
  are wired in: Checkout opens a Barion payment, and PAID is a `payments` row with its
  `payment_allocations`, not a flag the payer sets. Three paths settle it — the gateway's
  webhook, the payer's return to the invoice page (which re-asks the gateway, and is the only
  one that works with a `localhost` `BASE_URL`), and the `PAYMENT_SWEEP` job for a payer who
  never comes back. With
  `PAYMENT_BARION_POS_KEY` unset there is no gateway at all — no card button, and the only way to
  PAID is an operator recording the money through `POST /api/admin/payments`, for which the
  example seeds no account, so you reach it with a hand-made operator token.
- **A checkout mints a legal document on the buyer's say-so.** It runs against `sellers.id = 1`
  — the operator's own taxpayer id, from `SELLER_TAX_NUMBER` — so an org member's transfer
  checkout burns a number in the `EX` series and queues a NAV filing, with no password asked.
  That is deliberate: the alternative is a password prompt for something the customer did not
  ask for. The per-account rate tier bounds the rate and `MAX_QTY_E6` (24 units per booking)
  bounds the face value; a real consumer bills against its *own* taxpayer id, where minting a
  number for a sale the customer just agreed to is simply what a checkout is.
- **Email needs your own SMTP account.** Without one, activation and password-reset mails
  fail and you work from the token in the log.
- **One seller, one series, two services**, all seeded. There is no admin UI and no HTTP route
  that edits them: the seller's draft/publish methods live on the `Invoices` handle only, so
  master-data editing means code until `saas-admin` is built.
