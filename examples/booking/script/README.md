# `examples/booking/script` — the booking app, written in Rune

The twin of `../backend`. Same six routes at the same paths, same mounted framework bundles, so
`../frontend`'s one SPA build drives either — **and no Rust file in it**: five sources that
`saas-run` compiles at startup.

It is **not a Cargo crate** — not a workspace member, so `cargo test --all` and the pre-commit
hook do not cover it. Its suite runs through `saas-run test`.

`../backend` stays the primary contract. Where the two disagree, the Rust one is right and this
one is the comparison.

## Running it

```sh
nix-shell --run 'cargo build -p saas-run'          # from the repository root
(cd ../frontend && pnpm install && pnpm build)     # once; ../backend serves the same bundle
./target/debug/saas-run examples/booking/script    # serves on http://localhost:8082
```

Run it **from the repository root**: `DB_PATH` and `DATA_DIR` in `.env` are relative to the
process. `DIST_DIR` is relative to this directory, like `.env` itself, so `../frontend/dist` is
the same path `../backend/.env` uses.

The database lands in `./data/booking-script.db`, separate from `../backend`'s. That is not
optional: both mint invoice numbers under the same `EX` series, so one file would hand out the
same number twice. Recreating either burns the numbers it already filed. The script's own
tables live in the app database, `./data/booking-script/app.db` (`APP_DB_PATH` overrides it); an
older `script.db` there is renamed to `app.db` on boot.

`.env` is gitignored. The copy here documents every key. The app's own keys are `APP_`-prefixed
(`APP_SELLER_NAME`, `APP_NAV_LOGIN`, `APP_BASE_URL`, …): a script's `env::get` sees no other name,
so `backend/`'s unprefixed `SELLER_*` do not carry over.

`legal/` is a symlink to `../backend/legal`. `saas-run` publishes
`<app-dir>/legal/{terms,privacy}.md` at boot, versioned by each file's own sha256, and without
a published document
`consents_required` fails closed and every consent-gated route answers 403. One directory, two
applications, no drift.

## The tests

```sh
./target/debug/saas-run test examples/booking/script            # every case
./target/debug/saas-run test examples/booking/script checkout   # one, by name substring
```

15 cases, each with its own temporary file database and its own full build. They mirror
`../backend/tests/flow.rs` wherever the HTTP surface reaches.

**What the suite cannot reach**, and why: everything that drives the gateway by hand.
`saas-run test` registers no payment provider and `test::request` is HTTP-only, so there is no
way to move a payment into `Expired` from Rune. That costs
`a_live_card_payment_refuses_pay_by_transfer` and both `apply_state(Expired)` paths. The
`pay_by_transfer` happy path is a case; its locked-refusal half is not.

**What the suite hides.** `saas-run test` gives its account an OWNER membership on the ROOT org,
and that membership is exactly what `sys::escalate` exists to do without. Strip it and this
suite still passes 15/15 while `examples/invoicing/script` drops to 4/10 — that is the check
that proves the escalation, and it is worth re-running by hand after touching `checkout.rn`.

## How it diverges from `../backend`

Every one of these is deliberate.

| | Rust | Rune |
|---|---|---|
| **The checkout claim** | a `chk_<ULID>` claim is taken before the draft, settled after | **gone** |
| `A-BOOKING-ORPHANED` | an alert on a lost `settle` | gone with the claim |
| `E-BOOK-PAYMENT-OPEN` / `E-BOOK-PAYMENT-LIVE` | raised by `discard` / `pay_by_transfer` | `E-INV-LOCKED`, from the framework |
| an unknown payment provider | 400 before anything is drafted | the draft stands, no `redirectUrl` |
| line and list order | `occurred_on, id` | creation order (lines), newest-first (the list) |
| a stale list cursor | 404 `E-CORE-NOTFOUND` | 400 `E-CORE-VALIDATION` |
| bookings | a `bookings` table, a `BookingStore` trait, a migration module | one `app.object_type` declaration |

**The claim protocol is gone because one transaction makes it unnecessary, not as a compromise.**
Rust takes the claim first only because `Invoices::draft` owns its own transaction and `settle`
is a second write, so a crash between the two strands the set. `tx::with` spans both, and with
it go the claim token, `by_checkout`, the release-on-validation-failure path, `orphaned_claims`
and the alert. `a_note_the_draft_refuses_rolls_the_checkout_back` is the case that proves it:
the Rust twin needs a pre-check in `book` *plus* a release path in `checkout` to reach the end
state this reaches by rolling back.

**The two `E-BOOK-*` codes went** because `err::app` mints `E-APP-*` only and there is no
`refresh_invoice` binding to evaluate their condition with. `E-INV-LOCKED` says the same thing
and the SPA renders `errStr` and branches on neither code.

**An unknown provider is the one place this is weaker.** Rust resolves the gateway *before* it
drafts and answers 400; there is no binding here that lists the registered gateways, and
committing an invoice and then refusing the request would be worse than the missing 400. So a
gateway that is down, unregistered or misconfigured is no `redirectUrl` and nothing more —
which is what `../backend/src/bookings.rs::start_payment` already does for every other gateway
failure. With no POS key the SPA never offers CARD in the first place.

## Rune gotchas this file paid for

Each of these cost a debugging session.

- **A host binding *takes* its `String` argument**, and a field read hands out the same cell, so
  a uid used twice is empty on the second use. Read it once into a local and interpolate
  `` `${local}` `` at every call site — a local survives interpolation, a field does not.
- **A `tx::with` block must not end in a bare local.** Its tail expression is handed to the host
  as `Inline::Empty` rather than the value, and the caller sees
  `E-SCRIPT-RUNTIME: cannot serialize empty values`. Return `#{ uid }`, not `uid`.
- **A block must not end in a `for` loop** either, for the same reason with a different message.
- `Qty` is scaled 1e6 with no constructor from the scaled integer, so `qty_of_e6` builds the
  decimal string by hand — and `Qty` crosses out at full scale, `"2.500000"` rather than `"2.5"`.
- Rune's `Vec` has no `contains` and its `String` has no `repeat` or `is_ascii_digit`;
  `char::to_digit(10)` is the ASCII-digit test that does not also accept `٢`.
- **Function names are flat across the directory** — one root namespace per app, so a `book` in
  `bookings.rn` and a `book` fixture in `tests.rn` is a compile error.

## What is where

| file | |
|---|---|
| `main.rn` | the whole served surface — mounts, the six routes, the object type, the init hook — plus the shared helpers |
| `bookings.rn` | `book`, `list_bookings`, and the view that keeps the unbilled marker off the wire |
| `checkout.rn` | checkout, `pay_by_transfer`, `discard` — one `tx::with` each |
| `seed.rn` | `on_init`: the `sellers` row, its published version, the two catalogue services |
| `nav.rn` | the read-only NAV filing record |
| `tests.rn` | the 15 cases |
