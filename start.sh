#!/usr/bin/env bash
# Starts the example app: backend and esbuild in watch mode. Ctrl-C stops both.
# The backend serves the built SPA itself from DIST_DIR, so there is one origin.
#
#   backend   http://localhost:8080   <- open this one
set -euo pipefail
cd "$(dirname "$0")"

# `email.template_dir` defaults to `./templates/email`, which resolves against the backend's
# cwd (examples/booking/app-rust) and not the repo root — every SEND_EMAIL then fails on a
# missing file.
export EMAIL_TEMPLATE_DIR="$PWD/templates/email"

[ -d node_modules ] || pnpm install          # one workspace, installed from the root

trap 'kill 0' EXIT
# From examples/booking/app-rust, so DB_PATH=./data/booking.db and DIST_DIR=../frontend/dist
# resolve to the paths examples/booking/.gitignore covers.
(cd examples/booking/app-rust && nix-shell ../../../shell.nix --run 'cargo run -p booking') &
(cd examples/booking/frontend && pnpm watch) &
wait
