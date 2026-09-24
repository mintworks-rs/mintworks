# The booking example: one binary that serves the API and the built SPA.
# Build context is the repo root: `docker build -f examples/booking/Dockerfile .`

ARG UID=1000
ARG GID=1000

# FRONTEND BUILDER STAGE #
##########################

FROM node:24-slim AS frontend-builder
WORKDIR /app
COPY package.json pnpm-workspace.yaml pnpm-lock.yaml ./
COPY js/ ./js/
COPY examples/booking/frontend/ ./examples/booking/frontend/
RUN npm install -g pnpm \
	&& pnpm install --frozen-lockfile --filter saas-booking-frontend... \
	&& pnpm --filter saas-booking-frontend build

# RUST BUILDER STAGE #
######################

FROM rust:1-slim-trixie AS rust-builder
WORKDIR /app
COPY . .
# `sccache` is not in this image; the repo's .cargo/config may name it.
ENV RUSTC_WRAPPER=""
# 6, not 12: the release build runs on the same 6-core box as the desktop and `nice` cannot
# reach into BuildKit, so the cap is the number of jobs cargo starts. Overridable per build.
ARG CARGO_BUILD_JOBS=6
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
# `release-lto` already sets `strip = true`. libsqlite3-sys is bundled, but openssl (via
# `webauthn-rs-core`, saas-auth's passkeys) is not: the build needs its headers and library,
# which `shell.nix` selects for the dev shell too.
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev && \
	rm -rf /var/lib/apt/lists/*
RUN cargo build --profile release-lto -p saas-booking

ARG UID
ARG GID
# The scratch image has no shell: passwd/group and the data directory are made here.
RUN echo "app:x:${UID}:${GID}::/app/data:/sbin/nologin" > /etc/passwd.scratch && \
	echo "app:x:${GID}:" > /etc/group.scratch && \
	mkdir -p /appdata && chown ${UID}:${GID} /appdata

# FINAL STAGE #
###############

FROM scratch

# Re-declared: an ARG before the first FROM is global, but each stage has to opt in.
ARG UID
ARG GID

COPY --from=rust-builder /etc/passwd.scratch /etc/passwd
COPY --from=rust-builder /etc/group.scratch /etc/group
# `--chown`: COPY of a directory copies its *contents*, so `/app/data` is created here and never
# carried `/appdata`'s ownership. A named volume initialises from the image directory, so
# root-owned meant `USER app` could not open `DB_PATH` on first boot — on an image with no shell.
COPY --chown=${UID}:${GID} --from=rust-builder /appdata /app/data

WORKDIR /app

COPY --from=rust-builder /app/target/release-lto/saas-booking /usr/bin/saas-booking
# Only the mail templates: the typst invoice templates and the legal documents are
# `include_str!`-ed into the binary.
COPY --from=rust-builder /app/templates/email /app/templates/email
COPY --from=rust-builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=rust-builder /lib64/ld-linux-x86-64.so.2 /lib64/ld-linux-x86-64.so.2
# `libssl`/`libcrypto` come from `webauthn-rs-core` and pull in zlib and zstd, transitively; a
# `scratch` image has no shell to install any of them, so the binary fails to start without them.
COPY --from=rust-builder /lib/x86_64-linux-gnu/libgcc_s.so.1 /lib/x86_64-linux-gnu/libc.so.6 /lib/x86_64-linux-gnu/libm.so.6 /lib/x86_64-linux-gnu/libssl.so.3 /lib/x86_64-linux-gnu/libcrypto.so.3 /lib/x86_64-linux-gnu/libz.so.1 /lib/x86_64-linux-gnu/libzstd.so.1 /lib/x86_64-linux-gnu/
COPY --from=frontend-builder /app/examples/booking/frontend/dist /app/dist

ENV LD_LIBRARY_PATH=/lib
ENV DB_PATH=/app/data/booking.db
ENV DATA_DIR=/app/data
ENV DIST_DIR=/app/dist
ENV EMAIL_TEMPLATE_DIR=/app/templates/email
ENV LISTEN=0.0.0.0:80
ENV RUST_LOG=info
# No MASTER_KEY, BASE_URL or SELLER_* default: the app refuses to boot without them, which
# is the point — a generated key would decrypt nothing on the next start.

EXPOSE 80
VOLUME /app/data
USER app

CMD ["/usr/bin/saas-booking"]
