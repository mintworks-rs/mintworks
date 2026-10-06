# The invoicing application: `mintworks` over `app/`, serving the API and the built SPA.
# Build context is the repo root: `docker build -f examples/invoicing/Dockerfile .`

ARG UID=1000
ARG GID=1000

# FRONTEND BUILDER STAGE #
##########################

FROM node:24-slim AS frontend-builder
WORKDIR /app
COPY package.json pnpm-workspace.yaml pnpm-lock.yaml ./
COPY js/ ./js/
COPY examples/invoicing/frontend/ ./examples/invoicing/frontend/
RUN npm install -g pnpm \
	&& pnpm install --frozen-lockfile --filter invoicing-frontend... \
	&& pnpm --filter invoicing-frontend build

# RUST BUILDER STAGE #
######################

FROM rust:1-slim-trixie AS rust-builder
WORKDIR /app
COPY . .
# `sccache` is not in this image; the repo's .cargo/config may name it.
ENV RUSTC_WRAPPER=""
# Same cap and reason as `examples/booking/Dockerfile`: `nice` cannot reach into BuildKit.
ARG CARGO_BUILD_JOBS=6
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
# openssl via `webauthn-rs-core` (mintworks-auth's passkeys) is not bundled; libsqlite3-sys is.
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev && \
	rm -rf /var/lib/apt/lists/*
RUN cargo build --profile release-lto -p mintworks

ARG UID
ARG GID
# The scratch image has no shell: passwd/group and the data directory are made here.
RUN echo "app:x:${UID}:${GID}::/app/data:/sbin/nologin" > /etc/passwd.scratch && \
	echo "app:x:${GID}:" > /etc/group.scratch && \
	mkdir -p /appdata && chown ${UID}:${GID} /appdata

# FINAL STAGE #
###############

FROM scratch

ARG UID
ARG GID

COPY --from=rust-builder /etc/passwd.scratch /etc/passwd
COPY --from=rust-builder /etc/group.scratch /etc/group
# `--chown`: a named volume initialises from the image directory, and a root-owned one leaves
# `USER app` unable to open `DB_PATH` on first boot.
COPY --chown=${UID}:${GID} --from=rust-builder /appdata /app/data

WORKDIR /app

COPY --from=rust-builder /app/target/release-lto/mintworks /usr/bin/mintworks
# The Rune sources compile at startup, and `legal/` is published from beside them.
# `.dockerignore` keeps `app/.env` (MASTER_KEY) out of the context.
COPY --from=rust-builder /app/examples/invoicing/app /app/script
COPY --from=rust-builder /app/templates/email /app/templates/email
COPY --from=rust-builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=rust-builder /lib64/ld-linux-x86-64.so.2 /lib64/ld-linux-x86-64.so.2
COPY --from=rust-builder /lib/x86_64-linux-gnu/libgcc_s.so.1 /lib/x86_64-linux-gnu/libc.so.6 /lib/x86_64-linux-gnu/libm.so.6 /lib/x86_64-linux-gnu/libssl.so.3 /lib/x86_64-linux-gnu/libcrypto.so.3 /lib/x86_64-linux-gnu/libz.so.1 /lib/x86_64-linux-gnu/libzstd.so.1 /lib/x86_64-linux-gnu/
COPY --from=frontend-builder /app/examples/invoicing/frontend/dist /app/dist

ENV LD_LIBRARY_PATH=/lib
ENV DB_PATH=/app/data/invoicing.db
ENV DATA_DIR=/app/data
# Absolute: `dist_dir` joins it onto the app directory, which an absolute path replaces.
ENV DIST_DIR=/app/dist
ENV EMAIL_TEMPLATE_DIR=/app/templates/email
ENV LISTEN=0.0.0.0:80
ENV RUST_LOG=info
# No MASTER_KEY, BASE_URL, EMAIL_*, DEPLOYMENT_ENV or NAV_SOFTWARE_* default: each is
# deployment-specific, and a generated MASTER_KEY would decrypt nothing on the next start.
# Without DEPLOYMENT_ENV the image boots as production, which requires NAV_SOFTWARE_*.

EXPOSE 80
VOLUME /app/data
USER app

CMD ["/usr/bin/mintworks", "/app/script"]
