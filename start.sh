#!/usr/bin/env bash
# Starts an example: its backend and esbuild in watch mode. Ctrl-C stops both.
# The backend serves the built SPA itself from DIST_DIR, so there is one origin.
#
#   ./start.sh booking-rust|booking-rune|invoicing|research|subscription|agent-demo [case]
#   booking-rust  http://localhost:8080   examples/booking/app-rust
#   booking-rune  http://localhost:8082   the same app in Rune
#   invoicing     http://localhost:8081
#   research      http://localhost:8083   (`--features ai`; needs examples/research/app/.env)
#   subscription  needs examples/subscription/app/.env (LISTEN, BASE_URL, SMTP; see its README)
#   agent-demo    runs its test suite: its fakes are `app.test_default`s, which only
#                 `mintworks test` applies, so there is nothing to serve
set -euo pipefail
cd "$(dirname "$0")"

[ $# -ge 1 ] || { sed -n '5,/^set /{/^#/s/^# \{0,1\}//p}' "$0" >&2; exit 1; }   # the usage block above
app=$1

# `email.template_dir` defaults to `./templates/email`, relative to the process; exported so it
# holds whatever the app's `.env` resolves against.
export EMAIL_TEMPLATE_DIR="$PWD/templates/email"

if [ "$app" = agent-demo ]; then
	shift
	exec nix-shell shell.nix --run "cargo run -p mintworks --features ai -- test examples/agent-demo $(printf '%q ' "$@")"
fi

case $app in
booking-rust | booking-rune) dir=examples/booking appdir=$dir/app-${app#booking-} ;;
*) dir=examples/$app appdir=$dir/app ;;
esac
[ -d "$appdir" ] || { echo "no such example: $app" >&2; exit 1; }

features=
if [ "$app" = research ]; then
	features='--features ai'
	if [ ! -f "$appdir/.env" ]; then
		cp "$appdir/.env.example" "$appdir/.env"
		echo "Created $appdir/.env — fill in MASTER_KEY, LLM_API_KEY_DEEPSEEK," >&2
		echo "SEARCH_API_KEY_JINA and the EMAIL_SMTP_* settings, then run this again." >&2
		exit 1
	fi
fi

[ -d node_modules ] || pnpm install          # one workspace, installed from the root
# DIST_DIR is set and configured-but-missing is a deliberate startup failure, so a fresh
# checkout needs one build before the watcher's first rebuild lands.
[ -d "$dir/frontend/dist" ] || pnpm --filter "${dir#examples/}-frontend" build

if [ "$app" != booking-rust ]; then
	nix-shell shell.nix --run "cargo build -p mintworks $features"
fi

trap 'kill 0' EXIT
if [ "$app" = booking-rust ]; then
	# From app-rust, so DB_PATH=./data/booking.db and DIST_DIR=../frontend/dist resolve to the
	# paths examples/booking/.gitignore covers.
	(cd "$appdir" && nix-shell ../../../shell.nix --run 'cargo run -p booking') &
else
	target/nix-shell/debug/mintworks "$appdir" &
fi
(cd "$dir/frontend" && pnpm watch) &
wait
