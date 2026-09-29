#!/usr/bin/env bash
# Starts a Rune example: `saas-run` and esbuild in watch mode. Ctrl-C stops both.
# `saas-run` serves the built SPA itself from DIST_DIR, so there is one origin.
#
#   ./start-script.sh [booking|invoicing|research|agent-demo [case]]      (default: booking)
#   booking     http://localhost:8082
#   invoicing   http://localhost:8081
#   research    http://localhost:8083   (`--features ai`; needs examples/research/script/.env)
#   agent-demo  runs its test suite: its fakes are `app.test_default`s, which only
#               `saas-run test` applies, so there is nothing to serve
set -euo pipefail
cd "$(dirname "$0")"

app=${1:-booking}

# `email.template_dir` defaults to `./templates/email`, relative to the process; exported so it
# holds whatever the app's `.env` resolves against.
export EMAIL_TEMPLATE_DIR="$PWD/templates/email"

if [ "$app" = agent-demo ]; then
	shift
	exec nix-shell shell.nix --run "cargo run -p saas-run --features ai -- test examples/agent-demo $*"
fi

dir=examples/$app
[ -d "$dir/script" ] || { echo "no such example: $app" >&2; exit 1; }

features=
if [ "$app" = research ]; then
	features='--features ai'
	if [ ! -f "$dir/script/.env" ]; then
		cp "$dir/script/.env.example" "$dir/script/.env"
		echo "Created $dir/script/.env — fill in MASTER_KEY, LLM_API_KEY_DEEPSEEK," >&2
		echo "SEARCH_API_KEY_JINA and the EMAIL_SMTP_* settings, then run this again." >&2
		exit 1
	fi
fi

[ -d node_modules ] || pnpm install          # one workspace, installed from the root
# DIST_DIR is set and configured-but-missing is a deliberate startup failure, so a fresh
# checkout needs one build before the watcher's first rebuild lands.
[ -d "$dir/frontend/dist" ] || pnpm --filter "saas-$app-frontend" build

nix-shell shell.nix --run "cargo build -p saas-run $features"

trap 'kill 0' EXIT
target/debug/saas-run "$dir/script" &
(cd "$dir/frontend" && pnpm watch) &
wait
