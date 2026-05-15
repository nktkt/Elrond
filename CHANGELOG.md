# Changelog

All notable changes to Elrond are documented in this file.

## [0.15.0] - 2026-05-15

**TLS certificate hot-reload via `SIGHUP`.** 60 unit tests. Pre-alpha.

Closes the v0.6 caveat that TLS listeners kept their initial certificate
across reloads.

### Added

- **Cert hot-reload.** A `SIGHUP` re-reads the certificate and private
  key from disk (using the paths the config currently names), rebuilds
  a `rustls::ServerConfig` and `TlsAcceptor`, and pushes the new
  acceptor into the listener via a `watch::channel`. Connections that
  start after the reload use the new cert; in-flight TLS connections
  finish their handshake with whatever acceptor they captured at the
  point of `accept()`.
- **`TlsHandles`** — a small struct carrying `Arc<ServerConfig>` +
  `cert_path` + `key_path`. The supervisor uses the paths to re-read on
  reload; the `ServerConfig` is the initial live acceptor.
- A `watch::Sender<Arc<TlsAcceptor>>` lives in each TLS `HttpListener`;
  `server::run` watches the receiver and snapshots the latest acceptor
  per `accept()`.

### Verified end-to-end

Two self-signed certs (CN=`server-v1.local`, CN=`server-v2.local`)
sharing the same on-disk paths via `cp`. `openssl s_client` reported:
- **Before reload:** `subject=CN=server-v1.local`
- **After `cp v2.* live.*; kill -HUP`:** `subject=CN=server-v2.local`
- Body still served cleanly.
- Log: `reload: TLS listener 0.0.0.0:8443 re-loaded cert from
  /tmp/certs/live.crt / /tmp/certs/live.key`.

### Known follow-ups

- **Toggling TLS on/off in place** (adding `ssl` to a previously-plain
  listener, or vice versa) is logged but not yet handled. A restart is
  required for that change. The accept loop's plain vs. TLS branch is
  selected at spawn time.
- **SNI multi-cert.** One certificate per `server` block still — a
  resolver-based approach for SNI-routed multi-cert is the next
  closure of the TLS roadmap.
- **`ssl_protocols` / `ssl_ciphers` tuning** is still tolerated but
  unapplied.

## [0.14.0] - 2026-05-15

**`autoindex` directory listings.** 60 unit tests. Pre-alpha.

### Added

- **`autoindex on|off;`** at location level. When a request resolves
  to a directory with no `index.html`, Elrond renders an HTML listing
  instead of returning `404`.
- Listing format: minimal HTML5, sorted directories-first then by
  name, `../` link when not at the root of the location, percent-
  encoded `href` for entry names. Dotfiles (`.foo`) are skipped.

### Tests

- 60 unit tests (unchanged).
- **Smoke-tested:**
  - `GET /files/` rendered the HTML listing with subdirectories
    sorted before files; `.hidden` was correctly omitted.
  - `GET /files/alpha.txt` continued to serve the file directly
    (autoindex only activates on directories).
  - `GET /files/sub/` rendered a `../` link plus the subdir's
    contents.
  - A sibling location without `autoindex on;` still returned `404`
    for the directory request.

### Known limitations

- No size / mtime columns yet — name-only.
- No trailing-slash redirect (Nginx returns `301` for the no-slash
  form; Elrond serves it as if the slash were present).
- No styling toggles or `autoindex_format` JSON/XML modes.

## [0.13.0] - 2026-05-15

**Polish: `$scheme` honors TLS, server-level `add_header` cascade, `$host`
on HTTP/2.** 60 unit tests. Pre-alpha.

### Added

- **`$scheme`** now reflects the listener: `https` on TLS listeners,
  `http` on plain ones. Previously hard-coded to `http`. The new value
  flows through every template (`return` bodies, `proxy_set_header`,
  `add_header`, `proxy_cache_key`, `metrics`).
- **Server-level `add_header`** is now applied to every location in that
  server, in declaration order. Location-level `add_header` is applied
  last, so a location-level entry with the same name wins on conflict.
  This is the standard way to set `Strict-Transport-Security`,
  `X-Frame-Options`, `X-Content-Type-Options`, etc. once per server.

### Fixed

- **`$host` on HTTP/2.** hyper exposes the HTTP/2 `:authority` pseudo-
  header via `uri().authority()` rather than as a `Host` header.
  `RequestCtx::host()` now falls back to the URI authority when no
  `Host` header is present, so `$host` is non-empty for h2 requests.

### Tests

- 60 unit tests (was 59). Added `server_level_add_header_collected`.
- **Smoke-tested:** `$scheme` returned `http` over plain and `https`
  over TLS; server-level `add_header X-Service "elrond"` reached the
  response on both HTTP/2 and HTTP/1.1 listeners; a location-level
  `add_header X-Service "cors-specific"` overrode the server-level
  value; HTTP/2 `$host` rendered the full `localhost:8443` authority
  rather than empty.

## [0.12.0] - 2026-05-15

**Documentation deliverable (cross-cutting docs track).** No code
changes; 59 unit tests still pass. Pre-alpha.

### Added

- **[`docs/architecture.md`](docs/architecture.md)** — control plane /
  data plane separation, where each piece of state lives, request
  lifecycle from accept to access log, reload semantics, async / task
  model.
- **[`docs/compatibility.md`](docs/compatibility.md)** — directive-by-
  directive matrix for main, `http`, `server`, `location`, `upstream`,
  and `stream` contexts, plus the variable engine. Each entry is
  tagged ✅ implemented / 🟡 parsed-but-ignored / ❌ rejected.
- **[`docs/security-model.md`](docs/security-model.md)** — what Elrond
  defends against today, what it does not, what is the deployer's
  responsibility, and an explicit threat model summary.
- **[`docs/migration-from-nginx.md`](docs/migration-from-nginx.md)** — a
  procedure for moving an application from Nginx to Elrond, including a
  line-by-line action table for the common config patterns and a
  rollback discussion.
- README now links the four documents.

### Why this is its own version

The cross-cutting "Documentation" track in
[`ROADMAP.md`](ROADMAP.md) explicitly lists architecture,
compatibility, security-model, and migration-guide docs. Bundling them
behind a tagged version means the matrix in `compatibility.md` is
unambiguous about which Elrond it describes.

## [0.11.0] - 2026-05-15

**In-memory proxy cache MVP (Phase 9).** 59 unit tests. Pre-alpha.

Honest MVP: in-memory, single-zone semantics, with strict safety guards
in front of every insertion. The goal is to land caching *correctly*
first and grow toward Nginx parity.

### Added

- **`proxy_cache_path /path keys_zone=NAME:SIZE …;`** at http level.
  Other arguments (`levels=`, `inactive=`, `max_size=`, etc.) are parsed
  and accepted for forward compatibility but unused — v0.11.0 keeps
  everything in memory.
- **`proxy_cache <zone>;`** at location level.
- **`proxy_cache_key <template>;`** with variable interpolation. Default
  is `$scheme$host$request_uri`.
- **`proxy_cache_valid [code|any]… <duration>;`**, repeatable.
- **`X-Cache: HIT|MISS|BYPASS`** on every proxied response, so operators
  can grep for it.
- **Cache metrics** in `/metrics`:
  - `elrond_cache_hits_total`
  - `elrond_cache_misses_total`
  - `elrond_cache_bypass_total`
  - `elrond_cache_evicted_bytes_total`
  - `elrond_cache_bytes` (gauge)
  - `elrond_cache_entries` (gauge)
- Eviction: when an insertion would exceed `keys_zone` size, the
  soonest-to-expire entries are dropped until there's room. Bytes
  removed are reported via `elrond_cache_evicted_bytes_total`.

### Safety guards

A response is **never** cached when any of these hold (`X-Cache: BYPASS`
is emitted):

- Request method is not `GET`.
- Response has `Set-Cookie`.
- Response has any `Vary` header (Vary-aware variants are a follow-up).
- Response has `Cache-Control: no-store`, `private`, or `no-cache`.
- Response status does not match any `proxy_cache_valid` rule.
- Response body exceeds 4 MiB.

### Tests

- 59 unit tests (was 50). Added 9 cache tests covering: non-GET bypass,
  `Set-Cookie` bypass, `Vary` bypass, `Cache-Control: no-store` bypass,
  missing-valid-rule bypass, matching-status storage, store roundtrip,
  expired-on-read eviction, full-store eviction.
- **Smoke-tested end-to-end** against a counter backend:
  - First `GET /items` → `X-Cache: MISS`, counter=1, body stored.
  - Second `GET /items` → `X-Cache: HIT`, counter=1 (backend not hit).
  - Different URI `GET /other` → `X-Cache: MISS`, counter=2.
  - `/metrics` reported `hits=1, misses=2, entries=2, bytes=226`.

### Known limitations carried forward

- **No Vary-aware variants.** A response with `Vary` is bypassed entirely
  rather than cached per-variant.
- **No conditional revalidation** (`If-Modified-Since` / `If-None-Match`
  on cache fills). An entry expires when its TTL ends; we don't yet ask
  the upstream "still valid?"
- **No `proxy_cache_lock`.** Concurrent misses against the same key
  cause multiple upstream fills.
- **No `stale-while-revalidate` / `stale-if-error`.** Expired entries are
  not served while we re-fetch.
- **No disk persistence.** A restart loses the cache.
- **No purge endpoint.**
- **No range-aware caching.**

These are all known and on the roadmap.

## [0.10.0] - 2026-05-15

**On-the-fly gzip compression.** 50 unit tests. Pre-alpha.

### Added

- **`gzip on|off;`** at server and location level (with location overriding
  server).
- **`gzip_types <mime> …;`** at server level — adds MIME types to the
  built-in eligibility list.
- Built-in defaults (compressed when client offers `gzip`): `text/html`,
  `text/css`, `text/plain`, `text/javascript`, `text/xml`,
  `application/javascript`, `application/x-javascript`,
  `application/json`, `application/xml`, `application/atom+xml`,
  `application/rss+xml`, `image/svg+xml`, `font/woff`, `font/woff2`.
- Response gets `Content-Encoding: gzip`, an accurate compressed
  `Content-Length`, and a `Vary: Accept-Encoding` (appended cleanly to
  any existing `Vary`).
- **Skipped automatically** when: the client didn't offer `gzip`, the
  response already has a `Content-Encoding`, the response status is not
  one of `200/203/206/301/302`, the content type isn't eligible, or the
  body is shorter than 20 bytes (matches Nginx's `gzip_min_length`
  default).
- Tolerated, not yet applied: `gzip_disable`, `gzip_min_length`,
  `gzip_comp_level`, `gzip_proxied`, `gzip_vary`, `gzip_buffers`.

### Tests

- 50 unit tests (was 46). Added: `accept_detection`, `content_type_check`,
  `small_body_is_not_compressed`, `eligible_body_is_compressed`.
- **Smoke-tested on the wire:** a 225-byte plain payload compressed to
  75 bytes (3× reduction) when the client offered `Accept-Encoding: gzip`;
  `Vary: Accept-Encoding` was set; a 5-byte body stayed uncompressed;
  `curl --compressed` decoded the original content cleanly.

### Known limitations

- **Proxied responses are not yet gzip-eligible** — they stream, and
  v0.10.0 does not buffer to compress. Static and `return` responses
  cover most cases; proxied compression lands later.
- No Brotli yet (`br`).
- `gzip_min_length` is hard-coded at 20 bytes for v0.10.0 (matches
  Nginx default).
- Compression level is `flate2`'s default.

## [0.9.0] - 2026-05-15

**TCP `stream` proxy (Phase 10).** 46 unit tests. Pre-alpha.

### Added

- **`stream { … }` top-level block.** Parallel to `http`, with its own
  `upstream` and `server` sections. Reuses the same `Balancer` /
  `Peer` health machinery as HTTP.
  ```nginx
  stream {
      upstream db {
          server 127.0.0.1:5432 weight=2;
          server 127.0.0.1:5433;
      }
      server { listen 15432; proxy_pass db; }
  }
  ```
- **TCP forwarding** via `tokio::io::copy_bidirectional`. Bytes flow in
  both directions until either side closes.
- **All existing LB algorithms apply to stream traffic** — weighted
  round-robin, `least_conn`, `ip_hash` (keyed on the client's IP).
- **Passive health** carries over: `max_fails` / `fail_timeout` /
  `backup` / `down` work for stream peers exactly as for HTTP peers.
- **Supervisor now manages both kinds of listeners.** SIGHUP reload
  swaps both per-server HTTP state and per-stream-listener balancer
  atomically via separate `watch::channel`s; new stream addresses are
  bound, removed ones drain.
- **Stream metrics** in `/metrics`:
  - `elrond_stream_connections_accepted_total`
  - `elrond_stream_active_connections` (gauge, RAII guard)
  - `elrond_stream_bytes_client_to_upstream_total`
  - `elrond_stream_bytes_upstream_to_client_total`
- `Balancer::pick_for_addr(IpAddr)` API so the stream layer can pick a
  peer without an HTTP request context.

### Tests

- 46 unit tests (4 new): stream block parsing, stream server requiring
  both `listen` and `proxy_pass`, UDP rejection with a clear message,
  HTTP and stream coexisting in one config.
- **Smoke-tested end-to-end:** 9 TCP connections to a 2-backend pool
  with `weight=2`, `weight=1` produced exactly 6/3 distribution.
  Bidirectional bytes flowed through (`45` in / `189` out).
  `/metrics` reflected stream traffic accurately.
  HTTP and stream listeners coexisted in one process.

### Known limitations

- **TCP only.** `listen ... udp;` is parsed but explicitly rejected.
- No PROXY protocol (in or out).
- No SNI-based TCP routing for opaque TLS pass-through.
- Stream listeners are not yet enrolled in `GracefulShutdown` — drain
  signals new connections, but in-flight bidirectional copies finish
  on their own. Bounded by upstream connection lifetime.
- Stream metrics are global, not labeled per `upstream`.

## [0.8.0] - 2026-05-15

**Prometheus `/metrics` endpoint (observability cross-cut).** 42 unit tests.
Pre-alpha.

### Added

- **`metrics;` content directive** for a `location` block, exposing process
  metrics in Prometheus text-exposition format. Example:
  ```nginx
  location = /metrics { metrics; }
  ```
- **`src/metrics.rs`** — atomic counters and gauges updated from the
  request/connection hot path.
- **Metrics exposed:**
  - `elrond_build_info{version="…"} 1`
  - `elrond_uptime_seconds`
  - `elrond_requests_total`
  - `elrond_responses_total{status_class="1xx|2xx|3xx|4xx|5xx"}`
  - `elrond_connections_accepted_total`
  - `elrond_active_connections`
  - `elrond_proxy_attempts_total` (includes retries)
  - `elrond_proxy_failures_total`
  - `elrond_tls_handshakes_total`
  - `elrond_tls_handshake_failures_total`
- **RAII `ConnGuard`** auto-decrements the active-connections gauge when
  the connection task drops.
- Content-Type is the canonical `text/plain; version=0.0.4; charset=utf-8`
  so Prometheus scrapers recognize it without configuration.

### Tests

- 42 unit tests (one new: `metrics::tests::render_emits_required_metrics`
  validates every metric name appears, every `# TYPE` line names a
  documented metric type).
- **Smoke-tested end-to-end:** drove healthz traffic + proxy traffic +
  killed-backend traffic, then scraped `/metrics`. Counters reflected
  the observed traffic; `proxy_failures_total` matched the
  passive-health behavior (only the first attempt on a dead peer
  counts, subsequent picks are skipped until `fail_timeout`).

### Known limitations

- No per-server / per-upstream label dimensions yet — counters are
  process-global.
- No histograms (request latency) yet.
- No `/metrics` endpoint protection: anyone reaching the listener can
  scrape it. Authentication / IP allow-lists land later via an
  `allow`/`deny` directive.

## [0.7.0] - 2026-05-15

**HTTP/2 over TLS (Phase 7).** 41 unit tests. Pre-alpha.

### Added

- **HTTP/2 server**, negotiated via ALPN on TLS listeners. Same `service`
  is reused; per-stream multiplexing and HPACK are provided by hyper +
  `h2`.
- ALPN protocol order changed from `[http/1.1]` to `[h2, http/1.1]` —
  clients that don't know HTTP/2 still negotiate HTTP/1.1 cleanly.

### Changed

- TLS listener accept path now branches on the negotiated ALPN protocol
  after the handshake and chooses either `http1::Builder` or
  `http2::Builder` accordingly.
- `hyper` / `hyper-util` features extended with `http2`.

### Tests

- 41 unit tests (unchanged from v0.6.0).
- **Smoke-tested end-to-end:** the same TLS listener serves
  `curl --http2 https://...` as `HTTP/2 200` *and* `curl --http1.1
  https://...` as `HTTP/1.1 200`. Verbose curl output confirms ALPN
  selects `h2` when offered, opens a stream, and gets a clean response.

### Known limitations

- HTTP/2 over plaintext (h2c, prior-knowledge) is not implemented.
- gRPC `grpc_pass` is not yet wired — works through `proxy_pass` to an
  HTTP backend only.
- HTTP/2-specific directives (`http2_max_concurrent_streams`,
  `http2_max_field_size`, …) are not parsed yet.
- Rapid-reset / continuation-flood mitigations rely on `h2`'s defaults
  rather than explicit Elrond-level limits.

## [0.6.0] - 2026-05-15

**TLS / HTTPS (Phase 6).** 41 unit tests. Pre-alpha.

### Added

- **`listen ... ssl;`** is now honored. `ssl_certificate` and
  `ssl_certificate_key` are required for any TLS `server` block;
  configuration load fails fast if either is missing.
- **PEM loader** (`src/tls.rs`) for certificate chains (`certs`) and
  private keys (PKCS#8, PKCS#1, SEC1 — anything `rustls-pemfile`
  understands). Validates that at least one cert was found.
- **`rustls::ServerConfig` per `server` block** built at config-load time
  so misconfigured certs are reported at startup, not at the first
  handshake.
- **ALPN** advertises `http/1.1` (HTTP/2 over TLS lands in a later release).
- **TLS-aware logging:** `listening on https://…` for TLS listeners,
  `listening on http://…` for plain.
- The `rustls`'s "ring" crypto provider is installed once at process start.
- Several `ssl_*` directives (`ssl_protocols`, `ssl_ciphers`,
  `ssl_session_cache`, …) are now accepted by the parser for forward
  compatibility; they are not yet applied.

### Changed

- `app::Runtime::servers` is now `Vec<(SocketAddr, SharedState, Option<Arc<rustls::ServerConfig>>)>`.
- `server::run` takes an `Option<tokio_rustls::TlsAcceptor>`. On a TLS
  listener, each accepted connection is handshake'd in its own task to
  avoid stalling the accept loop.
- Supervisor reload preserves TLS state per listener (a config change to
  certs is logged but does not yet swap the live cert — see deferred).

### Tests

- 41 unit tests (one new test: `tls_listen_accepted_with_paths`; the old
  `rejects_tls_listen` was replaced with
  `tls_listen_requires_cert_paths`).
- **Smoke-tested end-to-end** with a self-signed `localhost` cert:
  `https://localhost:8443/` → `200`, exact-match `= /healthz` + `add_header`
  → 200 with `X-Service`, ALPN negotiates `http/1.1`, TLS 1.3 with
  AES-256-GCM-SHA384, plain HTTP coexists on 8080, plain HTTP to the
  TLS port fails the handshake (no accidental plaintext).

### Known limitations carried into v0.6.0

- One certificate per `server` block — multi-cert SNI selection is not
  yet implemented.
- **TLS connections are not yet enrolled in graceful shutdown** —
  in-flight TLS conns may be dropped on `SIGINT`/`SIGTERM`. Plain HTTP
  still drains correctly.
- **No certificate hot-reload on `SIGHUP`** yet — application config
  reload works, but a TLS listener keeps its certificate until restart.
- No OCSP stapling, no client certificate verification, no ACME.

### Still deferred (roadmap)

HTTP/2 (paired with TLS now we have ALPN), HTTP/3, caching, `stream`
proxying, active health checks, modules / WASM, OpenTelemetry exporter.

## [0.5.0] - 2026-05-15

Static-serving depth (Phase 3) and proxy retry (Phase 4). **40 unit tests**
(was 31). Pre-alpha; still no TLS.

### Added

- **Range requests** for static files: `bytes=start-end`, `bytes=start-`,
  `bytes=-suffix`. Returns `206 Partial Content` with `Content-Range` /
  `Content-Length`; out-of-range requests get `416 Range Not Satisfiable`.
  Multi-range and `multipart/byteranges` responses are deferred.
- **Weak ETags** for static files, built from `mtime` and size:
  `W/"<size>-<mtime>"`.
- **`If-None-Match`** conditional GET returns `304 Not Modified` when the
  client's ETag matches (including the `*` wildcard).
- **`Last-Modified`** header on every static `200` / `206` / `304`
  response.
- **`HEAD`** support: same headers and `Content-Length` as a `GET`, with no
  body.
- **`accept-ranges: bytes`** advertised on static responses.
- **`expires <duration>`** directive in `location` blocks. Sets both
  `Cache-Control: max-age=N` and an HTTP-date `Expires` header.
- **`proxy_next_upstream` retry** for idempotent methods (`GET`, `HEAD`,
  `OPTIONS`, `DELETE`). On connection errors *or* 5xx responses the
  request is forwarded to the next available peer, up to three attempts
  total. The failing peer is excluded from the retry pool and recorded as
  failed (so subsequent passive-health logic kicks in). Non-idempotent
  methods (`POST`, `PUT`, `PATCH`) still go to a single peer.
- Minimal IMF-fixdate formatter (`src/http_date.rs`), so we don't pull in
  a date crate for two header values.
- `Balancer::pick_excluding(ctx, exclude)` API used by the retry loop.

### Tests

- 40 unit tests, up from 31. Added: 7 for the range-spec parser
  (including suffix, open-end, clamped end, multi-range rejection, bad
  prefix) and 2 for the HTTP-date formatter (epoch, two known dates
  including a leap-day boundary).
- **Smoke-tested end-to-end** against a real backend:
  full GET → 200 with all the new headers; HEAD → same headers, no body;
  range 4-9 → `206 EFGHIJ`; suffix `-5` → `VWXYZ`; open `20-` → `UVWXYZ`;
  out-of-range `200-300` → `416`; ETag round-trip → `304`;
  retry on `proxy_pass` to a pool of `{dead, healthy}` → 5/5 `200`s.

### Still deferred

If-Modified-Since (date parsing), gzip, range coalescing, autoindex,
try_files semantics, TLS, HTTP/2/3, caching, `stream` proxying, active
health checks, `proxy_next_upstream` configurability. Roadmap unchanged.

## [0.4.0] - 2026-05-15

Graceful configuration reload (Phase 8). Pre-alpha; still no TLS.

### Added

- **`SIGHUP` reload.** Re-reads the configuration file, validates and builds
  it in full *before* swapping anything live. Mirrors Nginx semantics:
  - Listeners whose `listen` address is unchanged get the new state pushed
    via an atomic `watch::channel`. In-flight connections finish on the old
    state; brand-new connections on the same listener use the new state.
  - `listen` addresses added in the new config become brand-new listeners.
  - `listen` addresses removed from the new config are signaled to drain,
    in-flight requests finish, then the listener stops.
- **`SIGTERM` graceful shutdown** alongside the existing `SIGINT`.
- **`Supervisor`** subsystem (`src/supervisor.rs`) owns the lifecycle of all
  listeners. Each listener has its own state and shutdown `watch::Sender`,
  joined to the supervisor.
- Process-lifecycle log lines: PID at startup, signal received, listeners
  added/removed during reload, drain status.

### Changed

- `server::run` now takes a `TcpListener` and a `watch::Receiver<Arc<ServerState>>`
  instead of an owned state, so the supervisor can hand it new state without
  reopening the socket.
- `main.rs` slimmed down to argument parsing and signal handling; the lifecycle
  work lives in `Supervisor`.

### Behavior guarantees

- A **broken** new config never displaces a running config. The reload error
  is logged; the old listeners keep going.
- A reload that adds a listener whose port is already in use logs the bind
  error and continues with the rest of the configuration (the other
  listeners still get their state swap).

### Tests

- 31 unit tests (unchanged from v0.3.0; the reload pathway is exercised by
  the integration smoke test below — a proper integration-test harness
  comes in a later release).
- **Smoke-tested end-to-end:** `v1` → SIGHUP → `v2`; broken config + SIGHUP
  → still `v2`; add `listen 8091` + SIGHUP → both 8090 and 8091 serve;
  remove 8091 + SIGHUP → 8091 stops, 8090 serves `v4`; SIGTERM → clean
  shutdown.

### Still not implemented

`SIGUSR1` log reopen (no log file output yet — tracing writes to stdout),
`SIGUSR2` executable upgrade, TLS, HTTP/2/3, caching, `stream` proxying,
active health checks, `proxy_next_upstream`. Roadmap unchanged.

## [0.3.0] - 2026-05-15

Load balancing depth. **31 unit tests** (was 22). Pre-alpha; no TLS yet.

### Added

- **`least_conn`** load-balancing algorithm — picks the peer with the
  fewest in-flight requests, adjusted by weight.
- **`ip_hash`** load-balancing algorithm — requests from the same client
  IP stick to the same peer.
- **`max_fails=N`** and **`fail_timeout=Ns`** per upstream `server` line:
  consecutive failures up to `max_fails` mark the peer unavailable for
  `fail_timeout`. Cleared on the next successful response. (Nginx
  defaults: 1 / 10s.)
- **`backup`** flag — peer only chosen when all primaries are unavailable.
- **`down`** flag — peer never chosen.
- **Per-peer in-flight counter** with an RAII guard, dropped automatically
  when the request future is dropped.
- **Time-unit parser** for `fail_timeout`: bare seconds, plus `ms`, `s`,
  `m`, `h`, `d` suffixes.
- 5xx responses now count as upstream failures for passive health
  tracking; 2xx/3xx/4xx clear the failure counter.

### Changed

- `Balancer` now carries a per-peer health state and an `LbMethod` enum;
  picking iterates only available peers and falls back to backups.
- Weighted round-robin is computed per pick from the current set of
  available peers (so dead peers are skipped without rebuilding the pool).
- Direct-address `proxy_pass` targets (no `upstream` block) get sensible
  defaults: weight=1, max_fails=1, fail_timeout=10s.

### Tests

- 9 new unit tests: weighted-RR distribution, `least_conn` prefers idle
  peer, `ip_hash` stability, failed peer skipped during cooldown,
  `backup` only used when primaries fail, `down` peer never picked,
  duration-unit parsing, LB-method parsing, upstream-flag parsing.
- Runtime smoke-tested end-to-end: kill one of two backends → next
  requests get `502` once, then all routed to the healthy peer until
  `fail_timeout` elapses; after restart, both serve traffic again.

### Still not implemented

`proxy_next_upstream` retry, active health checks, generic `hash $var`,
consistent hashing, TLS, HTTP/2/3, hot reload, caching, `stream` proxy,
range requests, ETag/expires, regex location. Roadmap unchanged.

## [0.2.0] - 2026-05-15

Parser depth, variables, and proxy header control. Pre-alpha; still
single-binary, still no TLS.

### Added

- **Variable interpolation engine** (`src/template.rs`). Usable in `return`
  bodies, `proxy_set_header` values, and `add_header` values. Supports
  `$host`, `$remote_addr`, `$request_uri`, `$uri`, `$request_method`,
  `$args`, `$scheme`, `$server_name`, `$arg_<name>`, `$http_<header>`,
  `$cookie_<name>`, plus `${braced}` syntax. Unknown variables render as
  empty.
- **`include` directive** with relative-path resolution against the
  including file's directory and cycle detection.
- **Exact-match locations** (`location = /path`). Exact matches win over
  prefix matches, mirroring Nginx routing precedence. Other modifiers
  (`~`, `~*`, `^~`) are rejected with a clear error rather than silently
  ignored.
- **`proxy_set_header`** with full variable rendering at request time.
  Empty rendered values remove the header.
- **`add_header`** on responses, with variables.
- **`alias`** for static locations (filesystem path = alias + (URI -
  location prefix)).
- **Server-level `root` cascade** into locations that have no explicit
  content directive — matching Nginx's implicit-static behavior.
- Tolerated more no-op directives commonly found in real configs
  (`types`, `log_format`, `map_hash_*`, etc.) so existing Nginx configs
  load without stripping.

### Changed

- `Location` AST grew `kind`, `set_headers`, `add_headers`.
- Routing now goes exact → longest-prefix; prefix locations are sorted by
  length at load time so `route` returns on the first match.
- `proxy::forward` now applies hop-by-hop stripping → forwarding headers
  → `proxy_set_header` overrides, in that order.

### Tests

- 22 unit tests (was 7): template engine, exact-match parsing, alias,
  `proxy_set_header` / `add_header` collection, server-root cascade,
  `include` expansion via a real file, include cycle detection,
  unsupported `~` / `~*` / `^~` rejection.
- Runtime smoke-tested against a backend echo server confirming variables
  render correctly through `proxy_set_header`.

### Still not implemented (deferred to later releases)

TLS/HTTP2/HTTP3, hot reload (HUP), caching, `stream` proxying, active
health checks, `proxy_next_upstream` retry, `least_conn`/`ip_hash`,
`try_files`, `range` requests, `ETag`, `expires`, regex `location`,
`map`/`geo`, full `rewrite`, gRPC, ACME, observability exporters.

## [0.1.0] - 2026-05-13

First public release. Pre-alpha — not production-ready.

### Added

- Nginx-style configuration parser: lexer, recursive-descent parser, and a
  typed AST, with line-numbered syntax and semantic error messages.
- HTTP/1.1 server with keep-alive, built on Tokio + hyper.
- Prefix-based `location` routing with longest-prefix-wins matching.
- `return` directive for inline responses.
- Static file serving via `root`, with `index.html` directory fallback, a
  built-in MIME table, and path-traversal protection.
- Reverse proxy (`proxy_pass`) to a direct address or a named `upstream`,
  with request/response streaming and hop-by-hop header stripping.
- `X-Real-IP` and `X-Forwarded-For` injection on proxied requests.
- Weighted round-robin load balancing across `upstream` servers.
- Graceful shutdown on Ctrl-C: stops accepting and drains in-flight requests.
- Access logging and structured diagnostics via `tracing` (`ELROND_LOG` env).
- CLI: `-c/--config`, `-t/--test` (config check), `-v/--version`, `-h/--help`.

### Known limitations

- No TLS/HTTPS, HTTP/2, or HTTP/3 yet (`listen ... ssl` is rejected).
- No configuration hot-reload (`HUP`); a restart is required.
- No caching and no `stream` (TCP/UDP) proxying.
- `least_conn` / `ip_hash` are parsed but fall back to round-robin.
- Server-level `root`/`index` and `proxy_set_header` are accepted but not applied.
- Virtual hosts: each `server` binds its own `listen`; `server_name` is logged only.
- No `Range` requests, no `gzip`, no active health checks.

[0.15.0]: https://github.com/nktkt/Elrond/releases/tag/v0.15.0
[0.14.0]: https://github.com/nktkt/Elrond/releases/tag/v0.14.0
[0.13.0]: https://github.com/nktkt/Elrond/releases/tag/v0.13.0
[0.12.0]: https://github.com/nktkt/Elrond/releases/tag/v0.12.0
[0.11.0]: https://github.com/nktkt/Elrond/releases/tag/v0.11.0
[0.10.0]: https://github.com/nktkt/Elrond/releases/tag/v0.10.0
[0.9.0]: https://github.com/nktkt/Elrond/releases/tag/v0.9.0
[0.8.0]: https://github.com/nktkt/Elrond/releases/tag/v0.8.0
[0.7.0]: https://github.com/nktkt/Elrond/releases/tag/v0.7.0
[0.6.0]: https://github.com/nktkt/Elrond/releases/tag/v0.6.0
[0.5.0]: https://github.com/nktkt/Elrond/releases/tag/v0.5.0
[0.4.0]: https://github.com/nktkt/Elrond/releases/tag/v0.4.0
[0.3.0]: https://github.com/nktkt/Elrond/releases/tag/v0.3.0
[0.2.0]: https://github.com/nktkt/Elrond/releases/tag/v0.2.0
[0.1.0]: https://github.com/nktkt/Elrond/releases/tag/v0.1.0
