# subscription

The Rune application the commerce plans are exercised through: entitlements, the offer
catalogue, quote → checkout, subscriptions, coupons and rewards (commerce-3), and tier and seat
changes (commerce-4). It is not a workspace member, so `cargo test --all` does not cover it; its
suite runs through `saas-run`:

```sh
nix-shell --run 'cargo run -p saas-run -- test examples/subscription/script'
nix-shell --run 'cargo run -p saas-run -- test examples/subscription/script retried'  # one case
```

## Frontend

`../frontend` is the SPA over this app, built with `@saas-framework/client` only: Pricing (quote
→ confirm → checkout), Account (subscriptions, tier and seat changes with the quote shown before
confirming, cancel/resume, entitlements and meters), Invites (signup and affiliate links), and
Generate (the `ai_credits`-gated route and its 402 upsell). `/r/:code` is the link landing page;
it carries the code into registration as `ref`.

```sh
pnpm install                                            # from the repository root, never here
pnpm --filter saas-subscription-frontend build          # or `watch`
nix-shell --run 'cargo build -p saas-run'
./target/debug/saas-run examples/subscription/script      # from the repository root
```

Serving it needs what the suite does not: a `.env` here (`LISTEN`, `BASE_URL`, SMTP, and
`DIST_DIR=../frontend/dist`, relative to this directory as in `examples/booking/script/.env`) and
a `legal/` with `terms.md`/`privacy.md` — without published documents registration's consent
step fails closed. Card payments use the `barion` provider and its webhook needs `billing.public`
mounted; neither is set up here yet, so pay by transfer.

## Layout

- `main.rn` — `main(app)`:
  - entitlements `export` (feature), `seats` and `storage` (limits), `ai_credits` (meter);
  - offers: the `plan` family `free`/`pro`/`team` (monthly, ranks 0/1/2, `pro` with a 14-day
    trial), the `storage` add-on, the one-time `credits_1000`, and `reward_month_pro`, which is
    never sold and only granted as the `signup` ref reward on both sides (`plans.reward_on =
    first_payment`);
  - mounts `entitle`, `plans`, `refs`, `invoice.org_read`, `invoice.org_parties`, `billing.org`
    and `billing.operator`;
  - `seed` (`on_init`): a fixed demo seller and the `PLAN`/`STORAGE`/`CREDITS` services;
  - `POST /api/demo/generate` spends 10 `ai_credits`; `POST /api/demo/renew #{at}` runs the
    renewal sweep at an explicit unix time (operator only) — the suite's clock.
- `tests.rn` — credits spend to 402; a one-time purchase by transfer and by card (refunded, which
  cuts the grant); a card subscription settled by the scripted `fake` gateway and renewed through
  its stored recurrence; an unpaid renewal going `PAST_DUE` then `SUSPENDED`; cancel at period
  end; a trial with no invoice, then its first renewal; a 50% coupon for one period; and a
  `signup` ref rewarding inviter and invitee on the invitee's first payment (`test::signup`).

Grants and rewards are written by event handlers on their own tasks, so the suite re-reads
`/api/entitlements` until they land (`entitlements_when`). `/api/entitlements` evaluates at the
real clock, so a grace lapse is asserted through the subscription's status, not the balance.
