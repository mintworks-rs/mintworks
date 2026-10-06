# `@mintworks/client`

The framework's own browser client: transport, auth flows, wire types, money formatting and the
`E-*` error dictionaries. It is **headless** — it ships no CSS, no components and no page
layouts, because the framework does not own anyone's design system. Each application writes its
own markup and imports the parts below.

Two applications consume it today: `examples/booking/frontend` and
`examples/invoicing/frontend`. They share this package and share nothing else.

## What is deliberately not in here

No `Toast`, `Modal`, `DataTable` or `ui.tsx`; no Tailwind, no tokens, no theme. The one
component-shaped export, `ProtectedRoute`, ships the *branching* and takes every non-authenticated
state as a render prop (`loginPath?` `fallback?` `onError?` `consentGate?`) — the markup is the
consumer's. `examples/booking/frontend/src/auth/ProtectedRoute.tsx` is a wrapper supplying its four.

React Query hooks cover the **framework's own** routes only. An application's own routes — and
anything behind its own prefix — are built from the exported `api.*` verbs in the application.
Payment, NAV and any other feature hooks live in the application that needs them.

## Using it

```jsonc
// the consumer's package.json
"dependencies": { "@mintworks/client": "workspace:*" }
```

It is `private: true` and has no build step: `"exports": { ".": "./src/index.ts" }` points at raw
TypeScript, and the consumer's esbuild compiles it as if it were local source. A `tsc` build, a
`dist/` and a version only appear the day it is published to npm.

Three things a consumer must get right:

- **`"moduleResolution": "bundler"`** in `tsconfig.json`, or the `exports`-to-`.ts` mapping does
  not resolve.
- **`"es2023.intl"` in `tsconfig.json`'s `lib`** — that is where `Intl.StringNumericLiteral` and
  the string-accepting `NumberFormat.format` are declared, and the money surface formats amounts
  as strings so no amount ever becomes a float.
- **`src/pow/worker.ts` as its own esbuild entry point**, spelled `{ in, out: 'pow.worker' }` and
  served at `/pow.worker.js` (`POW_WORKER_URL`). It is a classic Worker; `entryNames: '[name]'`
  would flatten it to `worker.js` and the lookup would miss.

Peer dependencies: `react ^19`, `@tanstack/react-query ^5`, `react-router-dom ^7`.
`pnpm install` is run from the **repository root** — this is one pnpm workspace.

## The surface

| module | what it exports |
|---|---|
| `http` | `api.{get,post,put,patch,delete,blob}`, `ServerError`, `setOnAuthLost`, `errMsg` |
| `auth` | `AuthProvider`, `useAuth`, `ProtectedRoute`, `useConditionalPasskey`, `usePasskeyAvailable`, WebAuthn and cross-device QR helpers |
| `pow` | `solvePow`, `POW_WORKER_URL` |
| `money` | `formatMoney`, `formatQty`, `parseAmount`, `toQtyE6`, `vatRate`, `sumByCurrency`, `summaryTotals` |
| `commerce` | one function per refs, invitation, entitlement and plans route: `register`, `acceptInvite`, `previewRef`, `createRef`, `quote`, `checkout`, `subscriptionAction`, `adminCancel`, `reprice`, … |
| `errors` | `ERRORS_EN`, `ERRORS_HU`, `errText`, `fieldErrors` |
| `hooks` | React Query hooks and the `keys`/`urls` maps for the framework's routes, including `useOffers`, `useSubscriptions`, `useEntitlements`, `useCheckout`, `useSubscriptionAction`, `useRefs`, `useInvites` |
| `types` | the wire shapes |

Behaviour that is contract rather than implementation detail: the single refresh-and-retry, the
`Session` type dropping the token fields, and `keys.invoice(uid)` sitting under `keys.invoices`.

## Scripts

```sh
pnpm --filter @mintworks/client typecheck   # tsc --noEmit
pnpm --filter @mintworks/client lint        # biome
pnpm --filter @mintworks/client test        # vitest, the pure functions only
```

Nothing here is covered by `cargo test --all` or by `.git/hooks/pre-commit`.
