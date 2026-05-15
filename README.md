# Elrond

> A Rust-native Nginx alternative — built as a reverse proxy first, grown into a full web server.

Elrond is an experimental, ground-up reimplementation of Nginx in Rust. Rather than porting the C
codebase line by line, Elrond re-designs Nginx's **external behavior, configuration model, and
operational experience** on top of a modern async stack. The goal is not a perfect drop-in clone on
day one, but a server that can **safely replace Nginx for small-to-medium HTTP reverse-proxy
workloads** — and then grow outward from there.

## Status

🚧 **v0.6.0 — pre-alpha.** Not production-ready. APIs, configuration syntax, and crate layout will
change. See the [changelog](CHANGELOG.md) for what landed.

### What works in v0.6.0

- Nginx-style configuration parser with line-numbered errors (`elrond -t` to check a config)
- HTTP/1.1 server with keep-alive
- **TLS / HTTPS** via rustls — `listen 443 ssl;` + `ssl_certificate` / `ssl_certificate_key`;
  TLS 1.2 / 1.3; ALPN advertises `http/1.1`; plain HTTP and HTTPS coexist in one process
- Routing: exact-match `location = /path` plus longest-prefix `location /path`
- `include` directive (relative-path resolution, cycle detection)
- **Variable engine** — `$host`, `$remote_addr`, `$request_uri`, `$uri`, `$request_method`,
  `$args`, `$scheme`, `$server_name`, `$arg_*`, `$http_*`, `$cookie_*` — usable in `return`
  bodies, `proxy_set_header`, and `add_header`
- `return` directive for inline responses (with variables)
- Static file serving via `root` and `alias` — `index.html` fallback, MIME table, path-traversal
  protection, server-level `root` cascade, **Range requests, weak ETag, `If-None-Match` → 304,
  `Last-Modified`, `HEAD`, `expires`**
- Reverse proxy (`proxy_pass`) to a direct address or a named `upstream`, with streaming bodies
  and **`proxy_next_upstream` retry** on connect errors / 5xx for idempotent methods
- `proxy_set_header` and `add_header` with full variable rendering
- Load balancing: **weighted round-robin, `least_conn`, `ip_hash`**, with passive health
  (`max_fails` / `fail_timeout`), `backup`, and `down` peers
- `X-Real-IP` / `X-Forwarded-For` injection
- **Graceful `SIGHUP` reload** — validate new config, swap state on existing listeners, start
  new ones, drain removed ones; a broken new config never displaces a running config
- Graceful shutdown on `SIGINT` / `SIGTERM` (stops accepting, drains in-flight requests)

**Not yet:** TLS/HTTP2/HTTP3, config hot-reload, caching, `stream` proxying, health checks.
`listen ... ssl` is rejected rather than silently downgraded. Full list in the changelog.

## Why

Nginx is not "just an HTTP server" — it is an HTTP server, reverse proxy, content cache, load
balancer, TCP/UDP proxy, and mail proxy. Aiming for full compatibility from the start is a recipe
for failure. Elrond instead ships a focused HTTP/1.1 reverse proxy first, then layers on static
serving, TLS, HTTP/2, caching, `stream`, HTTP/3, configuration compatibility, and an extension
mechanism in well-defined phases.

## Goals

- Nginx-style configuration syntax (not TOML/YAML)
- HTTP/1.1, HTTP/2, and HTTP/3
- TLS termination with SNI and certificate hot-reload
- Reverse proxy with streaming, retries, and WebSocket upgrade
- Load balancing: round-robin, least-conn, ip-hash, passive/active health checks
- Content caching
- TCP/UDP (`stream`) proxying
- Graceful, zero-downtime reload (Nginx `HUP` semantics)
- First-class observability (`tracing`, metrics, OpenTelemetry)
- Extensibility via native Rust modules and WASM filters

**Non-goal:** binary or source compatibility with existing Nginx C modules.

## Architecture

Elrond separates a **control plane** (configuration, supervision, reload, worker lifecycle) from a
**data plane** (listeners, connections, routing, request handling). Request processing follows an
Nginx-style **phase engine**:

```
PostRead → ServerRewrite → FindConfig → Rewrite → PreAccess → Access
        → PostAccess → PreContent → Content → Log
```

Modules (`rewrite`, `access`, `proxy`, `static`, `log`, `rate_limit`, …) register handlers into
phases, so functionality can be added incrementally without touching the core.

## Tech stack

| Area              | Choice                  |
| ----------------- | ----------------------- |
| Async runtime     | Tokio                   |
| HTTP/1 & HTTP/2   | hyper                   |
| TLS               | rustls                  |
| HTTP/3 / QUIC     | quinn + h3              |
| Config parser     | hand-written lexer/parser + AST |
| Observability     | tracing, metrics, OpenTelemetry |

Cloudflare's [Pingora](https://github.com/cloudflare/pingora) is used as a design reference and
benchmark target rather than a dependency, since it does not aim for Nginx configuration
compatibility.

## Alpha target

The alpha milestone is reached when the following configuration runs correctly, passes basic
comparison tests against upstream Nginx, survives a reload under load without dropping requests,
and clears a baseline request-smuggling test suite:

```nginx
worker_processes auto;

events {
    worker_connections 4096;
}

http {
    access_log logs/access.log;
    error_log  logs/error.log;

    upstream app {
        server 127.0.0.1:3000 weight=2;
        server 127.0.0.1:3001;
    }

    server {
        listen 8080;
        server_name localhost;

        location /static/ {
            root ./public;
        }

        location / {
            proxy_pass http://app;
            proxy_set_header Host      $host;
            proxy_set_header X-Real-IP $remote_addr;
        }
    }
}
```

## Roadmap

The table below is the short version — see [ROADMAP.md](ROADMAP.md) for the long form, including
guiding principles, per-phase scope/non-scope, directive coverage, technical decisions, security
focus, completion criteria, cross-cutting tracks (observability, security, benchmarking,
documentation, platform), beyond-v1.0 ideas, non-goals, and the release-cadence mapping to v1.0.

| Phase | Focus                                  |
| ----- | -------------------------------------- |
| 0     | Research, design, compatibility scope  |
| 1     | Minimal HTTP/1.1 server                |
| 2     | Nginx-style configuration parser       |
| 3     | Static file serving                    |
| 4     | Reverse proxy MVP                      |
| 5     | Load balancing                         |
| 6     | TLS / HTTPS                            |
| 7     | HTTP/2                                 |
| 8     | Graceful / zero-downtime reload        |
| 9     | Caching                                |
| 10    | TCP/UDP `stream` proxy                 |
| 11    | HTTP/3 / QUIC                          |
| 12    | Module / plugin system                 |

### Compatibility levels

- **Level 0 — Nginx-like:** familiar syntax and behavior, not fully compatible.
- **Level 1 — Practical:** common reverse-proxy configs run with minor edits.
- **Level 2 — Operational:** usable as a production Nginx replacement.
- **Level 3 — Advanced:** complex configs (`stream`, HTTP/3, `map`, `geo`, regex locations, …) migrate.

## Repository layout

v0.1.0 ships as a single binary crate:

```
elrond/
  src/
    main.rs          # CLI, process lifecycle, graceful shutdown
    config/          # lexer, parser, AST, directive lowering
    app.rs           # runtime model (routing table, balancers)
    server.rs        # listener + connection handling
    proxy.rs         # reverse proxy + upstream connection pool
    static_files.rs  # `root` file serving
    body.rs          # shared body types
  examples/          # sample configurations
  public/            # demo static assets
  elrond.conf        # default configuration
```

### Future layout (planned)

As subsystems grow, the crate is expected to split into a workspace:

```
elrond/
  crates/
    elrond-cli/            # command-line entrypoint
    elrond-core/           # shared primitives
    elrond-config/         # lexer, parser, AST, validation
    elrond-runtime/        # master/worker, signals, reload, listeners
    elrond-http/           # connections, routing, phase engine
    elrond-proxy/          # upstreams, load balancing, connection pools
    elrond-static/         # static files, range requests, MIME
    elrond-tls/            # rustls integration, SNI, cert reload
    elrond-cache/          # cache keys, disk store, metadata
    elrond-stream/         # TCP/UDP proxying
    elrond-observability/  # logging, metrics, tracing
  examples/                # sample configurations
  tests/                   # config / integration / compatibility / fuzz
  benches/                 # http, proxy, static-file benchmarks
  docs/                    # architecture, compatibility, security
```

## Building

Requires a recent stable Rust toolchain (developed against Rust 1.95).

```sh
git clone https://github.com/nktkt/Elrond.git
cd Elrond
cargo build --release
```

## Running

```sh
# Validate a configuration
./target/release/elrond -t -c examples/load_balance.conf

# Run with the default ./elrond.conf (a hello-world + static demo)
./target/release/elrond

# Run a specific configuration
./target/release/elrond -c examples/minimal.conf
```

Sample configurations live in [`examples/`](examples/). Set `ELROND_LOG=debug` for verbose logs.

## Testing strategy

- **Config compatibility:** feed identical configs to upstream Nginx and Elrond, diff the behavior.
- **HTTP behavior:** keep-alive, chunked encoding, malformed headers, range, conditional GET, WebSocket upgrade.
- **Security:** request smuggling, response splitting, path traversal, header normalization, oversized headers, slowloris, cache poisoning.
- **Fuzzing:** config parser, HTTP request parser, URI/header normalizers, cache-key builder.
- **Benchmarks:** compared against Nginx, Envoy, HAProxy, and a Pingora sample proxy.

## Contributing

Elrond is in the design phase — issues and discussion about scope, compatibility decisions, and
architecture are especially welcome. Please open an issue before starting substantial work.

## License

Licensed under the [MIT License](LICENSE). Elrond is a clean-room reimplementation of Nginx
*behavior and specification*, not a translation of Nginx source code.
