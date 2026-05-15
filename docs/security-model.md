# Elrond Security Model

This document records what Elrond actively defends against, what it does
not, and where each control lives in the code. It's deliberately conserva-
tive: Elrond is still pre-alpha, and we'd rather be honest about gaps than
oversell the surface.

> **Coordinated disclosure.** Security issues should be reported privately
> via GitHub's "Report a vulnerability" feature on the repository, not via
> public issues.

---

## What we defend against today

### Path traversal

- **Where:** `src/static_files.rs::serve`.
- **How:** Every component of the request-derived path is inspected. Only
  `Component::Normal` and `Component::CurDir` are allowed. `..`,
  `RootDir`, `Prefix`, and `ParentDir` components are rejected before any
  filesystem call.
- **Returns:** `403 Forbidden`.

### Smuggling-relevant HTTP framing

- **Where:** `hyper` (HTTP/1.1) does the framing for us. We do not parse
  request lines or chunk boundaries by hand.
- **Hop-by-hop stripping:** `src/proxy.rs` removes
  `Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`,
  `Te`, `Trailers`, `Transfer-Encoding`, `Upgrade` in **both directions**.
  This is the same list as RFC 9110 §7.6.1.
- **Caveat:** We rely on hyper's enforcement of `Content-Length` /
  `Transfer-Encoding` rules. We do not yet have an Elrond-level fuzz
  harness exercising malformed framing — that lands as part of the
  cross-cutting fuzzing track.

### Cache safety

The proxy cache (`src/cache.rs`) refuses to store a response if any of
these hold, emitting `X-Cache: BYPASS`:

- Request method is not `GET`.
- Response has `Set-Cookie`.
- Response has any `Vary` header (Vary-aware variants are a roadmap
  item; until then we'd rather bypass than mis-serve).
- Response has `Cache-Control: no-store`, `private`, or `no-cache`.
- Response status does not match any `proxy_cache_valid` rule.
- Response body exceeds 4 MiB.

Cache keys are templates rendered against `RequestCtx`. The default key
includes `$scheme`, `$host`, and `$request_uri` so that the trivial
"different Host header → wrong cache lookup" mistake is one configuration
edit away.

### TLS

- **Backend:** `rustls` 0.23 with the `ring` crypto provider, installed
  once at process startup. We never fall back to OpenSSL.
- **ALPN:** advertised in order `h2`, `http/1.1`. Older clients negotiate
  HTTP/1.1 cleanly.
- **Refusal:** `listen ... ssl` without both `ssl_certificate` and
  `ssl_certificate_key` is a hard config error. Bad PEM files fail
  startup, not the first handshake.
- **Caveat:** Today we offer one cert per `server` block. SNI multi-cert
  resolution is on the roadmap. Until then, do not co-locate domains on
  one TLS listener.

### Configuration safety

- `SIGHUP` reload is validate-then-swap (`src/supervisor.rs`). A broken
  new config never displaces a running one.
- `listen ... ssl` is rejected — not silently downgraded to plaintext —
  when the cert paths are missing.
- Unknown directives are refused. Real-world Nginx configs that load
  modules we don't implement (`stream { ... } mail { ... }`) fail
  loudly rather than running with partial behavior.

### Headers

- `add_header` and `proxy_set_header` go through `HeaderValue::from_str`;
  invalid values are logged and skipped rather than panicking.
- We do not silently coerce CRLF or NUL bytes into headers (hyper
  enforces this); attempts to set such values are dropped with a debug
  log.

### Process lifecycle

- Graceful shutdown drains plain HTTP/1.1 connections via hyper-util's
  `GracefulShutdown`. TLS and stream-proxy connections are not yet in
  the same set (documented limitation).
- Both `SIGINT` and `SIGTERM` start the same drain.

---

## What we do not yet defend against

These are tracked in the roadmap and acknowledged here so deployments can
plan around them.

- **HTTP/2 abuse patterns** (rapid reset, continuation flood, HPACK
  bombs): we rely on `h2`'s defaults. Elrond-level limits and per-stream
  budgets are not yet exposed in configuration.
- **Vary-aware cache variants**: see the cache-safety section. Bypass is
  the conservative behavior today.
- **Cache poisoning by header injection**: we do not yet run a formal
  poisoning test suite in CI.
- **Active health checks** are not implemented; passive health
  (`max_fails` / `fail_timeout`) is.
- **TLS session resumption** uses rustls's in-process default; tickets are
  not persisted across restarts.
- **mTLS to upstream** (`proxy_ssl_*`) is not implemented.
- **OCSP stapling** is not implemented.
- **`auth_request` / `auth_basic`** are not implemented.
- **Rate limiting / connection limiting** (`limit_req`, `limit_conn`) are
  not implemented.
- **Continuous fuzzing** of the parser, request parser, URI / header
  normalizers, and cache-key builder is a roadmap deliverable. Unit
  tests cover the obvious cases; fuzzing is what catches the rest.

---

## What we leave to the deployer

- Network segmentation and firewall rules. Elrond binds where the config
  tells it to.
- TLS material rotation. Cert hot-reload on `SIGHUP` is on the roadmap;
  until then, restart on rotation.
- Authentication. Elrond can forward `Authorization` headers, set
  `X-Real-IP` / `X-Forwarded-For`, and route by path / variable — it does
  not itself authenticate clients.
- The `/metrics` endpoint is unauthenticated. Bind it on an internal
  listener (e.g., a dedicated `listen 127.0.0.1:9100` server) or front it
  with a `location` that rejects external IPs (once `allow` / `deny`
  lands).

---

## Threat model summary

Elrond's primary asset is the **integrity of the request/response flow**
between clients and upstreams: a request that reaches the upstream came
from the client that sent it, a response delivered to the client came from
the upstream we routed to, and nothing in between was rewritten in a way
the configuration does not authorize.

In-scope:

- A malicious client speaking HTTP/1.1, HTTP/2, or TLS.
- A malicious response from a backend (smuggling, header injection,
  oversized payloads).
- A typo in operator configuration (forgetting `ssl_certificate`,
  recursive `include`, …).

Out of scope today (treated as the deployer's responsibility):

- Compromised upstream identities (no mTLS / OCSP yet).
- DoS at the transport layer (no per-IP rate limits yet).
- Side-channel attacks on the host (timing leaks, cache eviction
  fingerprinting from outside the process).
- Compromise of the host running Elrond.
