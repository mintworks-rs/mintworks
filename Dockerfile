# The example application: one binary that serves the API and the built SPA.
# Build context is the repo root — the workspace is built in place, nothing is cloned.

ARG UID=1000
ARG GID=1000

# FRONTEND BUILDER STAGE #
##########################

FROM node:24-slim AS frontend-builder
WORKDIR /app
COPY example/frontend/ ./
RUN npm install -g pnpm && pnpm install --frozen-lockfile && pnpm build

# RUST BUILDER STAGE #
######################

FROM rust:1-slim-trixie AS rust-builder
WORKDIR /app
COPY . .
# `sccache` is not in this image; the repo's .cargo/config may name it.
ENV RUSTC_WRAPPER=""
# `release-lto` already sets `strip = true`. libsqlite3-sys is bundled and rustls is `ring`,
# so the build needs no system library beyond what rust:slim ships.
RUN cargo build --profile release-lto -p saas-example

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

COPY --from=rust-builder /app/target/release-lto/saas-example /usr/bin/saas-example
# Only the mail templates: the typst invoice templates and the legal documents are
# `include_str!`-ed into the binary.
COPY --from=rust-builder /app/templates/email /app/templates/email
COPY --from=rust-builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=rust-builder /lib64/ld-linux-x86-64.so.2 /lib64/ld-linux-x86-64.so.2
COPY --from=rust-builder /lib/x86_64-linux-gnu/libgcc_s.so.1 /lib/x86_64-linux-gnu/libc.so.6 /lib/x86_64-linux-gnu/libm.so.6 /lib/x86_64-linux-gnu/
COPY --from=frontend-builder /app/dist /app/dist

ENV LD_LIBRARY_PATH=/lib
ENV DB_PATH=/app/data/example.db
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

CMD ["/usr/bin/saas-example"]
