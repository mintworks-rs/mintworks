#!/usr/bin/env bash
# Starts the example app: mail sink, backend, esbuild in watch mode. Ctrl-C stops all three.
# The backend serves the built SPA itself from DIST_DIR, so there is one origin.
#
#   mailpit   SMTP 127.0.0.1:1025, inbox http://localhost:8025
#   backend   http://localhost:8080   <- open this one
set -euo pipefail
cd "$(dirname "$0")"

# Never sourced: `set -a` exported MASTER_KEY, SMTP_PASSWORD and the three NAV keys into
# `pnpm install`, where any dependency's lifecycle script can read them. The backend loads
# the file itself (dotenvy, example/backend/src/main.rs); this only peeks at one key.
envfile() { sed -n "s/^$1=//p" example/backend/.env 2>/dev/null | tail -1; }

# `email.template_dir` defaults to `./templates/email`, which resolves against the backend's
# cwd (example/backend) and not the repo root — every SEND_EMAIL then fails on a missing file.
export SAAS_EMAIL_TEMPLATE_DIR="$PWD/templates/email"

# No SMTP account configured -> talk to mailpit instead, so the activation link is readable
# in its inbox. Settings resolve row-then-env, and the example seeds no email row.
if [ -z "${SAAS_EMAIL_SMTP_HOST:-}$(envfile SAAS_EMAIL_SMTP_HOST)" ]; then
	export SAAS_EMAIL_SMTP_HOST=127.0.0.1 SAAS_EMAIL_SMTP_PORT=1025 \
		SAAS_EMAIL_SMTP_TLS_MODE=none SAAS_EMAIL_SMTP_USERNAME= \
		SAAS_EMAIL_FROM="${SAAS_EMAIL_FROM:-noreply@example.test}"
	nix-shell -p mailpit --run mailpit &
fi

[ -d example/frontend/node_modules ] || (cd example/frontend && pnpm install)

trap 'kill 0' EXIT
# From example/backend, so DB_PATH=./data/example.db and DIST_DIR=../frontend/dist resolve
# to the paths example/.gitignore covers.
(cd example/backend && nix-shell ../../shell.nix --run 'cargo run -p example-backend') &
(cd example/frontend && pnpm watch) &
wait
