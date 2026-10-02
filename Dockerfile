# syntax=docker/dockerfile:1

# Bandsnatch runs as a one-shot CLI; the schedule lives in docker/entrypoint.sh.
# That keeps `docker run <image> run --dry-run` working as a one-off and lets the
# same image be driven by cron or a K8s CronJob.

FROM rust:1-alpine AS builder

WORKDIR /usr/src/bandsnatch

# build-base: C toolchain for the bundled SQLite amalgamation and for `ring`.
# perl:      `ring`'s build script requires it.
# No cmake/openssl needed: Cargo.lock pins ring, not aws-lc-rs, and TLS is rustls.
RUN apk add --no-cache build-base perl

# Match the flake's musl build: a fully static binary so the same artefact runs
# on any libc. webpki-roots is compiled in, so the runtime image needs no
# ca-certificates.
#
# The build must pass `--target` explicitly. Without it, cargo applies RUSTFLAGS
# to host units as well, including proc-macro crates, and `+crt-static` cannot
# produce a proc-macro:
#   cannot produce proc-macro for `clap_derive` as the target
#   `aarch64-unknown-linux-musl` does not support these crate types
ENV RUSTFLAGS="-C target-feature=+crt-static"

# rust-toolchain.toml is not copied: it pins gnu/darwin targets for developer
# machines and CI, which are meaningless here and would make rustup install std
# for four unused targets.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY docs ./docs

# Deriving the triple from the host covers amd64 and arm64 without a Dockerfile
# per architecture.
RUN set -eux; \
    target="$(rustc -vV | sed -n 's/^host: //p')"; \
    cargo build --release --locked --target "$target"; \
    mkdir -p /out; \
    cp "target/$target/release/bandsnatch" /out/bandsnatch


FROM alpine:3 AS runtime

# su-exec: drop to PUID/PGID after fixing volume ownership.
# tzdata:  RUN_AT and log timestamps are local-time.
RUN apk add --no-cache su-exec tzdata \
    && addgroup -g 1000 bandsnatch \
    && adduser -D -H -u 1000 -G bandsnatch -s /sbin/nologin bandsnatch

COPY --from=builder /out/bandsnatch /usr/local/bin/bandsnatch
COPY docker/entrypoint.sh /entrypoint.sh
RUN chmod 0755 /entrypoint.sh

# Bandcamp credentials and the media library are mounted, not baked in.
ENV BS_OUTPUT_FOLDER=/music \
    BS_FORMAT=flac \
    INTERVAL=86400

WORKDIR /music
ENTRYPOINT ["/entrypoint.sh"]
CMD ["schedule"]
