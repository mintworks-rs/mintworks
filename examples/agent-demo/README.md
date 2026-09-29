# agent-demo

The Rune application the AI crates are exercised through. It is not a workspace member, so
`cargo test --all` does not cover it; its suite runs through `saas-run`, built with the `ai`
Cargo feature:

```sh
nix-shell --run 'cargo run -p saas-run --features ai -- test examples/agent-demo'
nix-shell --run 'cargo run -p saas-run --features ai -- test examples/agent-demo erase'  # one case
```

## Layout

- `main.rn` — `main(app)`: the `notes` table in the app DB, the routes, and the account hooks
  `app.on_account_export` / `app.on_account_erase`.
- `tests.rn` — one `#[test]` case per behaviour, loaded only by `saas-run test`. Each AI plan
  (`llm`, `memory`, `agent`, `search`) adds its own case here, and its declarations to `main.rn`.

## What it shows today

`notes` is personal data outside the framework's tables. The account export
(`GET /api/account/export`) carries it under the `"script"` key, and the erasure
(`POST /api/account/delete`) deletes it before the account is anonymised.

## Free live search with SearXNG

`web_search` defaults to Linkup, which needs a key. A self-hosted SearXNG instance needs none.
Enable its JSON API with a minimal `./searxng/settings.yml`:

```yaml
use_default_settings: true
server:
  secret_key: "change-me"
  limiter: false
search:
  formats: [html, json]
```

```sh
docker run -d -p 8888:8080 -v ./searxng:/etc/searxng searxng/searxng
```

Then run the demo with:

```sh
SEARCH_PROVIDER=searxng
SEARCH_BASE_URL_SEARXNG=http://127.0.0.1:8888
```

A 403 (`E-SEARCH-REJECTED`) means `json` is missing from `search.formats`.
