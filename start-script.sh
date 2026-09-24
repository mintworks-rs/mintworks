#!/usr/bin/env bash
# Starts a Rune example: `saas-run` and esbuild in watch mode. Ctrl-C stops both.
# `saas-run` serves the built SPA itself from DIST_DIR, so there is one origin.
#
#   ./start-script.sh [booking|invoicing]      (default: booking)
#   booking     http://localhost:8082
#   invoicing   http://localhost:8081
set -euo pipefail
cd "$(dirname "$0")"

app=${1:-booking}
dir=examples/$app
[ -d "$dir/script" ] || { echo "no such example: $app" >&2; exit 1; }

[ -d node_modules ] || pnpm install          # one workspace, installed from the root
# DIST_DIR is set and configured-but-missing is a deliberate startup failure, so a fresh
# checkout needs one build before the watcher's first rebuild lands.
[ -d "$dir/frontend/dist" ] || pnpm --filter "saas-$app-frontend" build

nix-shell shell.nix --run 'cargo build -p saas-run'

# `email.template_dir` defaults to `./templates/email`, relative to the process; exported so it
# holds whatever the app's `.env` resolves against.
export EMAIL_TEMPLATE_DIR="$PWD/templates/email"

trap 'kill 0' EXIT
target/debug/saas-run "$dir/script" &
(cd "$dir/frontend" && pnpm watch) &
wait
