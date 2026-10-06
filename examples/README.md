# examples/

Consumer applications built on the framework. Nothing under `crates/` or `adapters/` is touched
by anything here.

| | what it shows | run |
|---|---|---|
| `booking/backend` | a Rust consumer app: composition root, own SQL table and store trait, Barion card payments, NAV filing | `./start.sh` |
| `booking/script` | the same application in Rune, no Rust file — the comparison | `./start-script.sh` |
| `booking/frontend` | one React SPA; both backends serve it and answer the same routes | `pnpm --filter saas-booking-frontend watch` |
| `invoicing/script` | a Rune-only invoicing app: services, partners, invoices, projects | `./start-script.sh invoicing` |
| `invoicing/frontend` | a back-office SPA over it: dashboard, invoice workspace, master data, en + hu | `pnpm --filter saas-invoicing-frontend watch` |

`booking/backend` is the only workspace member, so it alone is covered by `cargo test --all` and
the pre-commit hook; the two Rune apps run their suites through `mintworks test <app-dir>`. See
each application's own README.

Both frontends are packages of the repository-rooted pnpm workspace and share
`@mintworks/client` (`js/client`) — so `pnpm install` is run from the repository root,
not from a frontend directory, and neither `cargo test --all` nor the pre-commit hook covers any
of the JavaScript.

## What the Rust/Rune comparison found

The same application, the same six routes, the same SPA: **1,323 lines of Rust src across six
files, 450 lines of Rune across five.** Two of the six Rust files have no counterpart at all —
`store.rs` (338 lines: the `bookings` DDL, the `BookingStore` trait and its impl, the migration
module) collapses into one `app.object_type` declaration, and `main.rs` (125 lines of
composition root) is `mintworks` itself. What is left is the same shape method for method.

The Rune side is also *shorter than its own logic*, because one thing genuinely disappears.
`bookings.rs` carries a checkout claim protocol — a `chk_<ULID>` token, `by_checkout`,
`release`, `orphaned_claims` and an `A-BOOKING-ORPHANED` alert — for one reason: `Invoices::draft`
owns its own transaction, so `settle` is a second write and a crash between the two bills a set
twice. `tx::with` spans both writes, and the condition the alert watches for becomes
unreachable. That is the strongest argument the scripting runtime has: not fewer lines, but one
fewer failure mode.

It is not free. The claim is also what lets Rust resolve the payment gateway *before* it drafts
and answer 400 for an unknown one; the script commits the invoice first and settles for no
redirect. Two `E-BOOK-*` codes become the framework's `E-INV-LOCKED` because there is no binding
to evaluate their condition with. The tests tell the same story — 15 cases against `flow.rs`'s
10, but three of `flow.rs`'s test a claim protocol that no longer exists and two need a payment
gateway driven by hand, which HTTP cannot reach. `booking/app-rune/README.md` lists every
divergence.
