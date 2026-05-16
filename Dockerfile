# Multi-stage build for Elrond. The final image is debian-slim rather
# than `distroless/static` because Elrond needs `libssl`-free runtime
# but does rely on the system trust store (/etc/ssl/certs) for HTTPS
# upstreams and on /etc/passwd for the `user` directive's `getpwnam`.
#
# Result: ~80 MB final image, single static-ish binary plus root CAs.
# For an even smaller image, swap the final stage for `gcr.io/distroless/cc-debian12`
# and accept that runtime `user nobody;` won't work without first
# mounting an /etc/passwd.

ARG RUST_VERSION=1.83

# ───────────────────────────── builder ───────────────────────────────
FROM rust:${RUST_VERSION}-slim-bookworm AS builder

WORKDIR /src

# Cache deps separately from source for fast incremental rebuilds.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
 && echo 'fn main() { panic!("placeholder"); }' > src/main.rs \
 && cargo build --release --bin elrond \
 && rm -rf src target/release/elrond target/release/deps/elrond-*

COPY src ./src
RUN cargo build --release --bin elrond \
 && strip target/release/elrond

# ───────────────────────────── runtime ───────────────────────────────
FROM debian:bookworm-slim AS runtime

# - `ca-certificates` for HTTPS upstream verification (rustls-native-certs
#   reads /etc/ssl/certs/ca-certificates.crt by default).
# - `tini` for proper signal forwarding (SIGTERM → graceful shutdown).
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini \
 && rm -rf /var/lib/apt/lists/*

# Create an unprivileged user the operator can drop to via
# `user elrond;`. uid/gid 101 matches the convention nginx uses on
# distroless variants so volume permissions translate cleanly.
RUN groupadd --system --gid 101 elrond \
 && useradd --system --uid 101 --gid 101 --home-dir /var/lib/elrond \
            --shell /usr/sbin/nologin elrond \
 && mkdir -p /etc/elrond /var/log/elrond /var/lib/elrond \
 && chown -R elrond:elrond /var/log/elrond /var/lib/elrond

COPY --from=builder /src/target/release/elrond /usr/local/bin/elrond

# Sensible default config — drops to the `elrond` user and serves a
# 200 on `/` so `docker run --rm -p 8080:8080 elrond` works out of the
# box. Operators override by mounting their own at `/etc/elrond/elrond.conf`.
COPY examples/docker-default.conf /etc/elrond/elrond.conf

# Note: we run as root so privileged ports work; the `user` directive
# in the config drops us at startup. To run rootless, set
# `--user elrond` and drop the `user` directive from the config.
EXPOSE 8080/tcp 8443/tcp 8443/udp

# tini reaps zombies and forwards SIGTERM → elrond → graceful drain.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/elrond"]
CMD ["-c", "/etc/elrond/elrond.conf"]
