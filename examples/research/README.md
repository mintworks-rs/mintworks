# `examples/research` — a research assistant, written in Rune

The application is the `app/` directory beside this file — the Rune sources `mintworks`
compiles at startup — plus the `frontend/` SPA it serves. A signed-in user asks a question; the
agent searches the web (Jina search + Jina Reader fetch) with DeepSeek as the LLM, answers in
markdown citing `[src_…]`, saves durable findings into the org's memory **notebook**, and any
notebook doc exports to PDF with a numbered Sources appendix. It exercises every AI crate —
`mintworks-llm`, `mintworks-agent`, `mintworks-memory`, `mintworks-search` — plus `mintworks-pdf`, against **real**
providers. The design record is `claude-docs/research-demo-design.md`.

It is **not a Cargo crate**, so `cargo test --all` and the pre-commit hook do not cover it, and
it needs the `ai` Cargo feature.

## Configuration

`app/.env` is gitignored; copy `app/.env.example` and fill it in. Beyond the bootstrap
values (`MASTER_KEY`, `DB_PATH`, `DATA_DIR`, `LISTEN`, `BASE_URL`, `DIST_DIR`) and the two
required mail settings `EMAIL_FROM` / `EMAIL_SMTP_HOST` (activation mail goes through them):

| variable | |
|---|---|
| `LLM_API_KEY_DEEPSEEK` | DeepSeek API key. The `research` profile is `deepseek:deepseek-flash`; base URL and price are compiled into `main.rn` |
| `SEARCH_PROVIDER=jina` | selects Jina search; the fetcher is already Jina by `mintworks` default |
| `SEARCH_API_KEY_JINA` | Jina key, shared by search and fetch. Reader fetch works keyless; search may not |
| `DEPLOYMENT_ENV=test` | the providers' **real sandboxes**, never fakes. Fakes exist only inside `mintworks test` (`app.test_default`) |

Each org gets a €1 LLM budget (`BUDGET_MICRO_EUR` in `main.rn`), set on its first thread. Past
it, a run ends with an `error` event carrying the `errCode`, which the chat shows.

## Running it

```sh
pnpm install                                        # from the repository root: one pnpm workspace
pnpm --filter saas-research-frontend build          # writes frontend/dist/
nix-shell --run 'cargo run -p mintworks --features ai -- examples/research/app'
                                                    # serves on http://localhost:8083
```

Run from the repository root: the `.env` paths are process relative, except `DIST_DIR`, which
is **app-dir** relative (`../frontend/dist`) — build the SPA before the first start, since a
configured path that is not a directory is a startup failure. The database is
`./data/research.db`, separate from the other examples'.

Register in the SPA, activate from the mailed link, and you work in your PERSONAL org — no
onboarding. `pnpm --filter saas-research-frontend watch` rebuilds on every edit.

## The tests

```sh
nix-shell --run 'cargo run -p mintworks --features ai -- test examples/research/app'
nix-shell --run 'cargo run -p mintworks --features ai -- test examples/research/app pdf'
```

Every route runs over the fake LLM and search providers (`test::llm_script`,
`test::search_fixture`), one temporary database and one full build per case.

## What is where

| file | |
|---|---|
| `app/main.rn` | mounts (`agent.runs`, `pdf.documents`), the `/api/app/*` routes, the citation renumbering for PDF |
| `app/tests.rn` | one case per route, on fakes |
| `app/packs/research/prompts/chat.en.md` | the system prompt: notebook first, search/fetch, cite, save findings |
| `app/templates/report.typ` | the PDF template (`title`, `body`) |
| `frontend/` | the SPA: threads, chat with streamed steps, notebook with version history and PDF export |
| `Dockerfile` | `docker build -f examples/research/Dockerfile .` from the repository root |
