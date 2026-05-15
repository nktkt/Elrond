# Elrond Architecture

This document describes how Elrond is organized today (v0.12.0). It is meant
to be the kind of overview an engineer reads before opening the source —
enough to know where to look for any given concern.

For *what* Elrond aims to do over the long arc, see
[`../ROADMAP.md`](../ROADMAP.md). For the matrix of which Nginx directives
work, see [`compatibility.md`](compatibility.md).

---

## Two planes

Elrond cleanly separates two responsibilities.

```
                ┌──────────────────────────────────────────┐
                │              Control plane               │
                │                                          │
   CLI args ──▶ │  config::load  ──▶  app::build           │
   signals  ──▶ │  Supervisor    (per-listener lifecycle)  │
                └──────────────────────────────────────────┘
                                  │
                                  │ runtime state via watch::channel
                                  ▼
                ┌──────────────────────────────────────────┐
                │              Data plane                  │
                │                                          │
   network  ──▶ │  HTTP listeners   ──▶  hyper + service   │
                │  Stream listeners ──▶  tokio io copy     │
                │                                          │
                └──────────────────────────────────────────┘
```

### Control plane

- **`config`** — lexer, parser, include expansion, AST validation.
  Produces a typed `Config`. Never touches sockets or threads.
- **`app::build`** — lowers `Config` into a `Runtime` of `(addr, state,
  optional TLS, optional cache, balancers)` tuples. Everything that needs
  to be ready before a packet arrives — cert chains parsed, balancers
  primed, cache zones allocated — happens here.
- **`Supervisor`** (`src/supervisor.rs`) — process-level orchestrator. Spawns
  one listener task per `listen` address. Owns the channels for live state
  swap on `SIGHUP` and shutdown signal on `SIGINT` / `SIGTERM`.

### Data plane

- **`server`** — HTTP/1.1 + HTTP/2 accept loop. Routes each request, runs
  the action, applies per-location headers, optionally gzips, records
  request metrics.
- **`stream`** — TCP accept loop. Picks a peer from the per-listener
  `Balancer` and bidirectionally pipes bytes.
- **`proxy`** — HTTP reverse proxy. Cache lookup → balancer pick → upstream
  request → 5xx-retry / passive-health bookkeeping → response. Buffers
  bodies for the cache when caching is enabled and the response is
  cacheable.
- **`static_files`** — `root` / `alias` serving with range, ETag, HEAD,
  conditional `If-None-Match`.

The two planes communicate only through `tokio::sync::watch::Sender` /
`Receiver` channels. A reload is, mechanically, the control plane sending
new state into those channels; the data plane reads it on the next accept.

---

## State that lives where

| Concept                         | Plane    | Module           | Notes                                    |
| ------------------------------- | -------- | ---------------- | ---------------------------------------- |
| Parsed configuration            | Control  | `config::ast`    | Typed, immutable, owned by `Supervisor`. |
| `Runtime`                       | Control  | `app`            | Bridge between control and data planes.  |
| `ServerState` (HTTP)            | Both     | `app`            | Shared via `Arc`, swapped on reload.     |
| `Balancer` + `Peer` health      | Both     | `app`            | Atomics; lock-free reads.                |
| `CacheStore` zones              | Both     | `cache`          | `Mutex<HashMap<…>>`; small enough.       |
| TLS `ServerConfig`              | Control  | `tls`            | Built once; not yet hot-reloaded.        |
| Metrics counters / gauges       | Both     | `metrics`        | Atomic statics; updated from hot paths.  |
| Per-connection in-flight count  | Data     | `metrics`        | RAII guard.                              |
| In-flight count per upstream    | Data     | `app::Peer`      | Atomic; RAII via `InflightGuard`.        |

The shared-across-planes pieces are all `Arc`-wrapped and use atomics for
mutability. We have not yet needed a single explicit lock outside the cache.

---

## Request lifecycle (HTTP)

1. **Accept.** `server::run` accepts a TCP connection. If the listener is
   TLS-enabled, it spawns a task to perform the rustls handshake; after
   handshake it inspects the ALPN protocol and chooses `http1::Builder` or
   `http2::Builder`.
2. **Connect upgrade or H1/H2 serve.** hyper's `serve_connection` drives
   the connection; each request invokes our `service_fn`.
3. **Snapshot context.** We clone the request's method, URI, and headers so
   the body can still be forwarded to a proxy. From these we build a
   `RequestCtx<'_>` for the variable engine.
4. **Route.** `ServerState::route` does exact match → longest-prefix
   match. Locations are pre-sorted at build time so this is `O(N_exact) +
   O(N_prefix)` with early exit.
5. **Run the action.**
   - `Return` renders the body template against the context.
   - `Static` resolves the FS path, checks traversal, checks `Range` /
     `If-None-Match`, builds a `200` / `206` / `304` / `404` response.
   - `Proxy` consults the cache (if any); on miss, picks a peer from the
     balancer, builds the upstream request, applies `proxy_set_header`,
     forwards via `hyper-util`'s legacy client. On 5xx / connection error
     for idempotent methods, retries the next peer.
   - `Metrics` renders the Prometheus exposition format inline.
6. **Apply per-location response headers.** `add_header`, `expires`.
7. **Gzip if eligible.** Static / `return` / `metrics` only; proxied
   bodies stream and are not yet compressed.
8. **Record.** Increment status-class counter, emit access log line.

---

## Reload semantics (SIGHUP)

`Supervisor::reload` enforces *validate-then-swap*:

1. Re-read the config file from disk; parse + build the new `Runtime`.
   If anything fails, log the error and return — the running process keeps
   serving on the old state.
2. Diff the new runtime against the current set of listeners:
   - **Address still present:** push the new state (and balancer, for
     `stream`) into the listener's `watch::Sender`. New connections see
     the new state on the next `accept()`. In-flight connections finish
     on the old snapshot.
   - **Address removed:** signal the listener to drain and stop.
   - **Address new:** bind it and spawn a fresh listener task.

A broken config never displaces a running one. A bind failure on a new
listener is logged but does not roll back the listeners that successfully
swapped.

---

## Async, threads, and tasks

Elrond is single-process and multi-threaded — Tokio multi-thread runtime.

- One task per listener accept loop.
- One task per accepted connection (HTTP and stream).
- One task per outstanding upstream request (managed by `hyper-util`'s
  legacy client connection pool).

We have not yet moved to an Nginx-style master / worker (multi-process,
`SO_REUSEPORT`) model. It's on the roadmap; some subsystems (cache, TLS
session resumption) become more interesting once worker isolation has a
cost.

---

## What is intentionally not here yet

Every roadmap item documented as deferred is genuinely absent — no half-
finished stubs, no silent downgrades. `listen ... ssl` without
`ssl_certificate` is a hard error; `listen ... udp` is a hard error; a
`Vary` response is bypassed by the cache rather than mis-cached. See the
[compatibility matrix](compatibility.md) for the directive-level status.
