# Contributing to Symbion Mintworks

## Sign-off (DCO)

Contributions come in under the [Developer Certificate of Origin](https://developercertificate.org/).
Sign off every commit with `git commit -s`, which adds a `Signed-off-by:` line certifying that
you may submit it under the project's licence.

## Development setup

`shell.nix` supplies the system dependencies: `pkg-config`, `libxml2` and libclang (the `libxml`
crate runs bindgen). The Rust toolchain comes from your system. Run every cargo command through
it:

```sh
nix-shell --run 'cargo test -p mintworks-invoice'
```

The shell sets `CARGO_TARGET_DIR=target/nix-shell`, so binaries land in `target/nix-shell/debug/`.
JavaScript is one pnpm workspace rooted at the repository: run `pnpm install` from the root.

## Gates

A pull request must pass:

```sh
cargo fmt --check --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

Every tracked `.rs`, `.ts`, `.tsx`, `.rn` and `.js` file must carry an SPDX header (see below).

The Rune applications under `examples/` are not crates, so `cargo test --all` does not cover
them. When you touch `crates/script` or `bin/mintworks`, run their suites
(`.github/workflows/ci.yml` is the authoritative list):

```sh
nix-shell --run 'cargo build -p mintworks --features ai,postgres'
./target/nix-shell/debug/mintworks test examples/booking/app-rune
./target/nix-shell/debug/mintworks test examples/invoicing/app
./target/nix-shell/debug/mintworks test examples/subscription/app
./target/nix-shell/debug/mintworks test examples/agent-demo
./target/nix-shell/debug/mintworks test examples/research/app
# PostgreSQL only:
PG_TEST_URL=… ./target/nix-shell/debug/mintworks test examples/pg-smoke
```

**PostgreSQL** tests need `PG_TEST_URL`, a role with `CREATEDB` and `CREATEROLE`; each test
creates and drops its own database. Unset, every PostgreSQL test skips and passes. With a
binary built with `--features postgres` and `PG_TEST_URL` set, `mintworks test` gives each case
fresh PostgreSQL databases; `examples/pg-smoke` runs only that way.

**JavaScript**: `pnpm -r typecheck`, `pnpm -r lint` and `pnpm --filter @mintworks/client test`.

## Code conventions

- **Formatting**: `cargo fmt` with hard tabs, width 100 (`rustfmt.toml`); JavaScript is formatted
  by biome. Rune has no formatter: tabs, by hand.
- Every `.rs` file ends with a `// vim: ts=4` modeline.
- Every source file starts with `// SPDX-License-Identifier: MPL-2.0`, or `MIT-0` under
  `examples/`; after a `#!` line if there is one.
- `#![forbid(unsafe_code)]`. No `unwrap`/`expect`/`panic` outside tests.
- **No floats.** Money is `Money(i64)` minor units, quantities are scaled 1e6, VAT rates are
  integer basis points. VAT is computed once per rate group on the summed net, never per line.
- **An `ISSUED` invoice is immutable.** Every update touching amounts, lines or the buyer
  snapshot is guarded by `status = 'DRAFT'`; numbers are allocated only inside the issue
  transaction.
- Route handlers deserialize, call one service method and serialize; validation, authorization
  and transactions live in the service handle.
- A schema change is three edits in **each** store adapter: the DDL in `schema.rs`, an
  `if from < N` block in `migrations.rs`, and the bumped `schema::VERSION`.

### Comments

Comments say **why**, not what: the constraint a future reader would otherwise break, the
rejected alternative and the failure it caused. One reason per comment, and three lines is the
default ceiling for an inline block. Never restate the line below. Where a regression test
exists, the test is the guard and the comment a one-line signpost. Doc comments on public items
may run longer: they are the contract a second adapter is implemented from.
