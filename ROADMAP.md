# Elrond Roadmap

This document is the long-form companion to the brief phase table in the README.
It records, for every planned phase of Elrond, what is in scope, what is *not*,
the directives we intend to honor, the technical decisions we expect to make,
and the conditions under which the phase is considered complete.

The roadmap is a living document. Phase boundaries, durations, and even
ordering may shift as we learn — but the **shape** of the journey (start as a
focused HTTP/1.1 reverse proxy, grow outward into TLS, HTTP/2, caching,
`stream`, HTTP/3, and an extension system) is deliberately fixed.

---

## Guiding principles

A small number of decisions drive every phase. When in doubt about scope, fall
back to these.

1. **Behavior compatibility over source compatibility.** We re-implement what
   Nginx *does* and how its configuration *reads*. We do not port C code, and
   we do not aim to load C modules.
2. **Reverse proxy first.** The HTTP/1.1 reverse proxy is the focal use case
   through v1.0. Everything else — TLS, HTTP/2, caching, `stream`, HTTP/3 — is
   layered onto a proxy core that we keep healthy at every step.
3. **Correctness before performance.** Request smuggling, header
   normalization, path traversal, and cache poisoning are first-class
   concerns. A faster server that desynchronizes a backend is not a faster
   server.
4. **Operational ergonomics matter.** Graceful reload (HUP), log rotation,
   `systemd` integration, and clear error messages are part of the product,
   not nice-to-haves.
5. **Explicit refusal beats silent downgrade.** If we cannot honor a
   directive safely yet (`listen ... ssl` in v0.1.0, for example), we error
   out at config-load time rather than serving plaintext.
6. **One subsystem at a time.** Each phase produces a tagged release. We do
   not merge a half-finished cache while we are also rewriting the parser.

---

## Compatibility levels

Each phase advances Elrond along a four-step compatibility scale. Where a
phase fits is called out in its section.

### Level 0 — Nginx-like

The configuration *reads* like Nginx and the *behavior* is recognizable, but
non-trivial configs need rewriting. v0.1.0 is here.

Covered: `http`, `server`, `location`, `proxy_pass`, `upstream`, `root`,
`return`, basic `access_log` / `error_log`.

### Level 1 — Practical

Common reverse-proxy configurations run with only minor edits. Most teams
using Nginx as a front door for an app can switch.

Adds: `proxy_set_header`, `proxy_buffering`, `try_files`, `rewrite`, full
`access_log` formats, TLS, HTTP/2, working `index` / `alias`.

### Level 2 — Operational

Elrond is a viable production Nginx replacement.

Adds: graceful reload, log rotation, upstream keepalive, passive health
checks, caching, rate limiting, connection limiting, `systemd` integration,
metrics & tracing.

### Level 3 — Advanced

Complex, real-world Nginx setups migrate.

Adds: `stream` (TCP/UDP), HTTP/3, `auth_request`, `mirror`, subrequests,
`map`, `geo`, regex `location`, full variable evaluation, an extension
mechanism (native Rust modules + WASM filters).

---

## Phase 0 — Research, design, compatibility scope

**Status:** ✅ Substantially complete as of v0.1.0.

**Goal.** Decide *what we are not building* before we build anything.

**Outputs already in the repo.**
- This roadmap and the README, which together define the v1.0 scope.
- A working subset of Nginx config grammar (`src/config/`).
- An explicit non-goal: binary or source compatibility with Nginx C modules.

**Outputs still owed.**
- `docs/architecture.md` — control-plane / data-plane separation, the phase
  engine, where each subsystem lives.
- `docs/compatibility.md` — a directive-by-directive matrix: implemented,
  parsed-but-ignored, rejected.
- `docs/security-model.md` — threat model: what attacks we defend against,
  what we leave to the deployer.
- `docs/benchmark-plan.md` — how we measure, what we measure against
  (Nginx, Envoy, HAProxy, Pingora sample proxy).

**Completion criteria.** Every later phase can point at this folder when
asked "why is X out of scope?" or "what's the security model?"

---

## Phase 1 — Minimal HTTP/1.1 server

**Status:** ✅ Shipped in v0.1.0.

**What landed.**
- TCP listener per `server` block, accepting on a configurable address/port.
- HTTP/1.1 with keep-alive (via `hyper`).
- `listen`, `server_name` (logged), `location` (prefix match), `return`.
- Access log via `tracing` (target `access`), error/diagnostic log via
  `tracing` (`ELROND_LOG` env filter).
- Graceful shutdown on `SIGINT`: stop accepting, drain in-flight requests via
  `hyper_util::server::graceful::GracefulShutdown`.
- CLI: `-c/--config`, `-t/--test`, `-v/--version`, `-h/--help`.

**Compatibility level.** L0.

**Open follow-ups.** Header / URI fuzzing harness (deferred to Phase 4 along
with the smuggling test suite).

---

## Phase 2 — Nginx-style configuration parser

**Status:** ✅ Core shipped in v0.1.0. Several large features still to come.

**What landed.**
- Lexer with line tracking, quoted strings, and `#` comments.
- Recursive-descent parser producing a generic `Directive` tree.
- Lowering to a typed AST with context validation (e.g. `location` inside
  `server` inside `http`).
- Line-numbered errors for unknown directives, missing arguments,
  unterminated strings, and unclosed blocks.
- A handful of tolerated-but-unused directives so that real-world configs do
  not need to be stripped to load.

**What v1.0 still needs.**
- `include` actually following the path and inlining the result, with cycle
  detection.
- Variables: `$host`, `$remote_addr`, `$request_uri`, `$args`, `$arg_*`,
  `$cookie_*`, `$http_*`, `$uri`. A small interpolation engine usable in
  `return`, `proxy_set_header`, `add_header`, `rewrite`, and `access_log`
  formats.
- `map` and `geo` directives feeding the variable engine.
- `regex` `location` matchers (`~`, `~*`, `^~`, `=`) with the correct
  Nginx-matching precedence.
- `if` (with the well-known caveats documented).
- Configuration *merging*: server-level `root`, `index`, `access_log`, and
  `proxy_set_header` flowing into locations the way Nginx does.

**Compatibility level reached.** L1 once variables and merging land.

**Completion criteria.**
- Syntax errors point at the right line, with the right column where cheap.
- Round-tripping a non-trivial config (`include`, `map`, regex `location`)
  produces the same observable behavior on Elrond as on Nginx.
- Fuzz target running in CI on the lexer/parser/normalizer.

---

## Phase 3 — Static file serving

**Status:** Partial in v0.1.0. Phase 3 closes the gap.

**What landed.**
- `root` resolution following Nginx semantics (root + full URI path).
- `index.html` fallback for directory requests.
- Built-in MIME table.
- Path-traversal rejection at the component level.

**What Phase 3 adds.**
- `alias` (replace the matched prefix, not append).
- `try_files` with the full Nginx fallback semantics, including `=404` and
  named-location targets.
- `index` accepting multiple candidates.
- `autoindex` for directory listings (off by default).
- `Range` requests, including multipart byte-range responses.
- Conditional requests: `If-Modified-Since`, `If-None-Match`, `ETag`.
- `Cache-Control` / `Expires` via `expires`.
- `types` directive (load additional MIME mappings from the config).
- Sendfile-style fast paths where the OS supports it: `sendfile(2)` on
  Linux, `io_uring` behind an opt-in feature flag, `mmap` for medium files.
  Behind a feature flag and never the default until we have benchmarks.
- `gzip` and `gzip_static` (precompressed `.gz` neighbor files).

**Security.** Path traversal continues to be checked by component; the
canonicalized result must remain inside the configured root. Symlink
following is configurable and off by default once we get to L2.

**Completion criteria.**
- A 1 KB / 1 MB / 1 GB file all serve correctly.
- `Range` returns `206` with the right `Content-Range`.
- `alias` and `root` produce paths identical to Nginx on a shared test
  corpus.

---

## Phase 4 — Reverse proxy MVP → full

**Status:** MVP shipped in v0.1.0. The hardening work is the heart of Phase 4.

**What landed.**
- `proxy_pass` to a direct address or a named `upstream`.
- Request/response streaming using `hyper-util`'s legacy client.
- Hop-by-hop header stripping (RFC 9110 §7.6.1) in both directions.
- `X-Real-IP` / `X-Forwarded-For` injection.

**What Phase 4 adds.**
- `proxy_set_header`, `proxy_hide_header`, `proxy_pass_header`,
  `proxy_pass_request_headers`.
- `proxy_redirect`, `proxy_cookie_domain`, `proxy_cookie_path`.
- Request and response buffering (`proxy_buffering`, `proxy_buffers`,
  `proxy_buffer_size`, `proxy_busy_buffers_size`, `proxy_request_buffering`).
- Per-route timeouts (`proxy_connect_timeout`, `proxy_send_timeout`,
  `proxy_read_timeout`).
- Per-route retry: `proxy_next_upstream` (`error`, `timeout`, `invalid_header`,
  `http_500`, …), `proxy_next_upstream_tries`,
  `proxy_next_upstream_timeout`.
- `proxy_intercept_errors` and `error_page`.
- WebSocket / `Upgrade` end-to-end forwarding.
- HTTP/2 upstream (paired with Phase 7) and `grpc_pass`.
- A formal smuggling test suite, derived from the public catalog plus the
  cases Pingora hardened against. CI gates merge on this.
- Header / URI normalization with strict and lenient modes; strict is the
  default.

**Security focus.** This is the phase where Elrond either earns or loses the
right to be called a production proxy. Specific items:

- Single `Content-Length`, no `Transfer-Encoding` mixing.
- Strict `chunked` parsing.
- No HTTP/1.0 connection reuse for ambiguous responses.
- Header name and value validation that does not differ between the public
  and backend sides.
- Aggressive testing of `100-continue`, trailers, and request body framing.

**Compatibility level reached.** L1.

**Completion criteria.**
- Node, Go, Rails, and a slow Python backend all proxy correctly.
- Smuggling test suite green in CI.
- Streaming a multi-gigabyte upload to an upstream uses bounded memory.
- WebSocket upgrade survives a 1-hour soak test under graceful reload.

---

## Phase 5 — Load balancing

**Status:** Partial in v0.1.0 (weighted round-robin only).

**What landed.**
- Weighted round-robin via address replication.
- A `Balancer` trait shape that other algorithms can plug into.

**What Phase 5 adds.**
- `least_conn` (with active connection counters).
- `ip_hash` and `hash <key> [consistent]` (generic and consistent hashing).
- Server attributes: `max_fails`, `fail_timeout`, `backup`, `down`,
  `slow_start`.
- Passive health checks: a server marked `failed` is skipped for
  `fail_timeout`, mirroring Nginx open-source behavior.
- Active health checks (Plus-style): periodic probes against a configurable
  URI, including expected status and body match. Optional.
- `keepalive`, `keepalive_requests`, `keepalive_timeout` for upstream pools.
- Per-balancer metrics: pick count, in-flight, latency histograms,
  consecutive-failure counters.
- DNS resolution for `server` entries: `resolver` directive, periodic
  re-resolution, in-place set updates.

**Compatibility level reached.** L1 → trending toward L2.

**Completion criteria.**
- Each algorithm distributes traffic to within 1% of its expected share
  under steady load.
- A failing backend is taken out and reinstated as expected.
- Connection-pool metrics line up with `netstat`-observed reality.

---

## Phase 6 — TLS / HTTPS

**Status:** Not started. `listen ... ssl` is rejected.

**Why we waited.** A bad TLS rollout is worse than no TLS. We do this after
the proxy core is hardened so that we are not bug-fixing two large systems at
once.

**What Phase 6 ships.**
- `listen 443 ssl;` and the `ssl_*` directive family:
  `ssl_certificate`, `ssl_certificate_key`, `ssl_protocols`, `ssl_ciphers`,
  `ssl_prefer_server_ciphers`, `ssl_session_cache`, `ssl_session_timeout`,
  `ssl_session_tickets`, `ssl_dhparam`, `ssl_ecdh_curve`, `ssl_stapling`.
- SNI-based certificate selection.
- ALPN negotiation (`h2`, `http/1.1`) — the actual HTTP/2 handler lands in
  Phase 7, but the listener already advertises it.
- Certificate hot-reload that does not drop in-flight connections.
- Backed by `rustls`, with the OS trust store via `rustls-native-certs`.

**Phase 6.5 (parallel track).**
- An `openssl-backend` feature flag for environments that require FIPS or
  specific cipher policies that `rustls` does not yet cover.
- ACME / Let's Encrypt integration as a separate `elrond-acme` module: HTTP-01
  challenge served from a reserved `location`, on-disk certificate store,
  background renewal.

**Compatibility level reached.** L1 fully.

**Completion criteria.**
- HTTP/1.1 over TLS works against `curl`, browsers, and `openssl s_client`.
- A `Sec-` mis-configuration is reported at config-load, not at the first
  handshake.
- Certificate rotation under load is verified with `wrk` running across the
  swap.

---

## Phase 7 — HTTP/2

**Status:** Not started.

**What Phase 7 ships.**
- HTTP/2 server, negotiated via ALPN `h2`.
- HTTP/2 upstream when the backend supports it (and `grpc_pass`).
- Per-stream flow control, stream multiplexing, header compression
  (HPACK) — provided by `hyper`'s HTTP/2 implementation.
- Directives: `http2`, `http2_max_concurrent_streams`,
  `http2_max_field_size`, `http2_max_header_size`,
  `http2_recv_timeout`, `http2_idle_timeout`.
- `grpc_pass`, `grpc_set_header`, `grpc_read_timeout`, etc.
- A defense-in-depth pass on known HTTP/2 abuse patterns: rapid reset
  (CVE-2023-44487-class), continuation flood, header compression bomb. All
  must be reproducible in CI and bounded by configurable limits.

**Compatibility level reached.** L1 fully + start of L2 (gRPC use case).

**Completion criteria.**
- `h2load` produces clean baselines.
- A gRPC health-check call is proxied end-to-end without buffering the
  trailers.
- HTTP/1.1 ↔ HTTP/2 conversion in both directions is round-trip-safe for
  the headers we forward.

---

## Phase 8 — Graceful / zero-downtime reload

**Status:** Not started. `SIGINT` triggers a clean shutdown today, but
configuration changes require a restart.

**Why this is its own phase.** Reload is where most "Rust Nginx" projects
quietly stop being honest. We want it right.

**What Phase 8 ships.**
- `SIGHUP` re-reads the configuration. On parse / validation failure, the
  process keeps running with the old config and reports the error.
- On success, listeners that already exist keep their accepting sockets;
  new listeners are bound (or report errors and abort the reload).
- Workers serving the old config drain naturally; new connections immediately
  use the new config.
- `SIGUSR1` reopens log files (for log rotation).
- `SIGUSR2` performs an executable upgrade, mirroring Nginx semantics where
  possible (the new binary inherits sockets via env-vars / file-descriptor
  passing).
- `SIGQUIT` is the canonical graceful shutdown.
- `SIGTERM` is fast shutdown (in-flight requests get a short deadline).
- A PID-file directive and a `systemd` unit with `Type=notify` support
  (`sd_notify(READY=1)` and `RELOADING=1`).

**Compatibility level reached.** L2 in the operational sense.

**Completion criteria.**
- 1000 requests/second sustained across a reload, zero dropped, zero 502s
  except those manufactured by the test.
- An invalid config never displaces a valid running config.
- `logrotate` with `postrotate /bin/kill -USR1 $(cat /run/elrond.pid)` works.

---

## Phase 9 — Caching

**Status:** Not started.

**Why caching is hard.** This is the largest single-phase risk in the
roadmap. Caching is where correctness, performance, and security all
intersect, and where a small bug becomes a leaking-customer-data incident.

**What Phase 9 ships.**
- `proxy_cache_path` with disk store, an in-memory metadata index, and
  configurable inactive-time / max-size eviction.
- `proxy_cache`, `proxy_cache_key`, `proxy_cache_valid`,
  `proxy_cache_use_stale`, `proxy_cache_background_update`,
  `proxy_cache_revalidate`, `proxy_cache_min_uses`, `proxy_cache_lock`.
- `proxy_cache_bypass` and `proxy_no_cache` for per-request decisions.
- `proxy_cache_purge` (a cache-purge endpoint, gated by `allow`/`deny`).
- Conditional revalidation (`If-Modified-Since`, `If-None-Match`).
- Vary handling that does not silently coalesce variants.
- Stale-while-revalidate and stale-if-error semantics.
- Range-request caching that fills holes from upstream rather than
  re-fetching whole files.
- Cache-poisoning test suite: cache-key smuggling, header injection,
  Vary mismatches, response splitting attempts.
- Crash-resistant on-disk format: the cache survives an unclean shutdown
  without serving truncated entries.

**Compatibility level reached.** L2 fully.

**Completion criteria.**
- A static-asset workload sees a measurable hit ratio improvement.
- A poisoning test suite (public catalog + project-specific cases) is green
  in CI.
- Deleting an arbitrary file from the cache directory does not crash Elrond
  on next access.

---

## Phase 10 — `stream` (TCP/UDP) proxy

**Status:** Not started.

**What Phase 10 ships.**
- A top-level `stream { … }` block, parallel to `http`.
- TCP proxying with the same `upstream`/`server`/`listen` shape.
- UDP proxying (DNS, QUIC-as-passthrough, syslog).
- SNI-based routing for TLS pass-through (route by `ClientHello` SNI without
  terminating TLS).
- PROXY protocol v1 and v2 (in and out).
- Connection and bandwidth limits (`limit_conn`, `proxy_upload_rate`,
  `proxy_download_rate`).
- Per-stream access logs and metrics.
- Stream-level load balancing (round-robin, least-conn, hash).

**Compatibility level reached.** L3 (this is one of the L3 features).

**Completion criteria.**
- PostgreSQL, Redis, and an opaque TCP service all proxy correctly with no
  observable corruption.
- A DNS UDP proxy round-trips authoritative responses without buffering
  glue records.
- A SNI-routed multi-tenant TLS frontend reaches the right backend with no
  TLS termination on Elrond.

---

## Phase 11 — HTTP/3 / QUIC

**Status:** Not started.

**What Phase 11 ships.**
- QUIC endpoint, UDP listener integrated with the existing control plane.
- HTTP/3 server, via `quinn` + `h3`.
- HTTP/3 reverse proxy (`proxy_pass`) speaking HTTP/3 upstream when the
  backend supports it, otherwise transparently downgrading to HTTP/2 or 1.1.
- `Alt-Svc` advertisement so HTTP/1.1 / HTTP/2 clients discover HTTP/3.
- 0-RTT support, with safe-method / replay guidance documented and an opt-in
  switch.
- A connection-migration test (client IP/port change mid-stream) that
  passes.
- QUIC-specific limits and timeouts: max idle, max streams, congestion
  controller selection (CUBIC, BBR-when-available).

**Compatibility level reached.** L3 advances substantially.

**Completion criteria.**
- `curl --http3` reaches a static page and a proxied path.
- HTTP/3 → HTTP/1.1 backend works under load.
- A path-MTU change does not stall a long-lived stream.

---

## Phase 12 — Module / plugin system

**Status:** Not started. Decisions are pending; this section will firm up as
the lower phases settle.

**What we are choosing between.**
1. **Native Rust modules.** Compiled in. Highest performance, tightest type
   safety, no sandbox cost. Cost: every plugin pins to a specific Elrond
   ABI/release.
2. **WASM filters.** Plugins shipped as `.wasm`, executed in a sandboxed
   runtime (Wasmtime). Good safety story, language-agnostic, distribution
   friendly. Cost: API design is the real work, performance is bounded by
   the runtime.
3. **Embedded scripting.** Rhai or Starlark for small policy code in the
   config. Cheap to embed, comfortable for users, slow for hot paths.
4. **Lua compatibility layer.** Targets OpenResty users; very expensive to
   build and to maintain.

**What we will likely ship.**
- Phase 12a — native Rust module API. Stable trait surface for `HttpModule`,
  `StreamModule`, phase registration, variable providers, log handlers.
  Internal subsystems (`proxy`, `static`, `cache`, …) reorganize behind it
  first; only then is it published.
- Phase 12b — WASM filter API for request/response transformation, modeled
  on a minimal subset of the Envoy/Proxy-Wasm shape but our own. Sandboxed
  with deterministic time and resource limits.
- Phase 12c (optional) — embedded Rhai for tiny policy snippets in the
  config (e.g. a header rewrite that needs a conditional).
- Lua: a tracking issue with prior art and effort estimates, but not on the
  v1.0 path.

**Compatibility level reached.** L3 fully (extension story).

**Completion criteria.**
- At least one shipping module is implemented against the public API
  rather than the internal one.
- A WASM filter performing a non-trivial header rewrite survives a fuzzing
  pass and a perf benchmark.
- API stability promise documented (semver, deprecation window).

---

## Cross-cutting tracks

These are not single phases. They run alongside.

### Observability

- `tracing`-based structured logs across every subsystem.
- Prometheus metrics exporter: connection counters, request latency
  histograms, upstream pool gauges, cache hit/miss rates.
- OpenTelemetry tracing for proxied requests; trace context propagation
  (`traceparent`, `b3`).
- Per-`server` and per-`upstream` metric labels.

### Security

- Continuous fuzzing of the parser, request parser, URI normalizer, header
  normalizer, and cache-key builder via `cargo-fuzz` in CI.
- Smuggling test suite kept up-to-date with new disclosures.
- A documented threat model in `docs/security-model.md`.
- A coordinated-disclosure policy in `SECURITY.md`.

### Benchmarking

- Reproducible benchmark harness comparing Elrond, Nginx, Envoy, HAProxy,
  and a Pingora sample proxy.
- Scenarios: static 1 KB / 1 MB, reverse proxy 1 KB / 100 KB, TLS handshake
  rate, keep-alive throughput, HTTP/2 multiplexing, slow client, backend
  failure, reload under load.
- Results are reproducible from the repo, not just published numbers.

### Documentation

- A directive matrix kept in `docs/compatibility.md`, regenerated from the
  parser tables.
- An "Operations" guide covering reload, log rotation, `systemd`,
  certificate management.
- A "Migration from Nginx" guide that takes a real-world Nginx config and
  walks through the diff.

### Platform

- Linux first (the only target supported through v0.x).
- macOS for development.
- A path to Windows once the core stabilizes (post-v1.0).
- FreeBSD as a stretch target.

---

## Beyond v1.0

These are not v1.0 commitments. They are ideas we want to capture before
we forget.

- **`auth_request` and `mirror`.** Already on the L3 list; concrete design
  needed.
- **Rate limiting and connection limiting** (`limit_req`, `limit_conn`)
  with token-bucket and leaky-bucket variants, per-key, per-zone shared
  memory replaced with a Rust-native shared store.
- **Built-in WAF hooks.** Not a full WAF, but a clean integration point for
  ModSecurity-style rule engines or in-process detection.
- **mTLS to upstream.** `proxy_ssl_certificate` and friends.
- **Edge compute.** WASM filters running ahead of routing for A/B logic,
  feature flags, request shaping.
- **eBPF integration** on Linux for accept-path acceleration, fine-grained
  connection accounting, and zero-copy paths.
- **Multi-process model.** Today Elrond is single-process, multi-threaded.
  An Nginx-style master/worker split with `SO_REUSEPORT` becomes interesting
  once the cache and TLS subsystems land and per-worker isolation becomes
  worth the bookkeeping.
- **Embedded admin API.** A small read-only HTTP API on a dedicated
  listener exposing health, metrics, and a config preview.
- **GitOps reload.** Watch a config directory; on change, validate and
  trigger an in-process reload.

---

## Non-goals

Some things we are intentionally *not* doing, to keep the project honest.

- **Source compatibility with Nginx C modules.** Not in v1.0, not on the
  visible horizon.
- **A drop-in replacement for every Nginx directive.** Some directives
  encode C-specific concerns or rarely-used quirks that we do not plan to
  reproduce.
- **A mail proxy.** Nginx ships `mail { … }`. Elrond does not, and probably
  will not.
- **A general-purpose application server.** Elrond is a proxy/server. CGI,
  FastCGI, and SCGI are not on the v1.0 path. `proxy_pass` to a process is
  the supported pattern.
- **A scripting host for arbitrary user code.** WASM filters and a small
  embedded scripting option, yes; loading and running unrestricted Rust
  plugins in-process, no.

---

## Release cadence and versioning

Until v1.0, Elrond uses `0.x.y` versions where each minor release advances
the roadmap by roughly one phase. Patch releases (`0.x.y → 0.x.(y+1)`)
carry fixes only.

A tentative mapping — the line moves as phases land:

| Version  | Phase(s) delivered                  |
| -------- | ----------------------------------- |
| v0.1.0   | Phases 1, 2, 3, 4, 5 (initial)      |
| v0.2.0   | Phase 2 (full), Phase 3 (full)      |
| v0.3.0   | Phase 4 (full), Phase 5 (full)      |
| v0.4.0   | Phase 6 — TLS                       |
| v0.5.0   | Phase 7 — HTTP/2                    |
| v0.6.0   | Phase 8 — Graceful reload           |
| v0.7.0   | Phase 9 — Caching                   |
| v0.8.0   | Phase 10 — `stream` TCP/UDP         |
| v0.9.0   | Phase 11 — HTTP/3                   |
| v0.10.0  | Phase 12 — Module / WASM            |
| **v1.0** | Stable API surface, frozen subset   |

v1.0 is reached when:

1. The directive matrix is stable and documented.
2. Reload, TLS, HTTP/2, caching, and basic observability are all in.
3. The smuggling test suite, the cache-poisoning test suite, and the
   fuzzing harness are all green in CI and have stayed green across at
   least one full minor release.
4. A migration guide exists for a representative real-world Nginx config.
5. Performance, on a published benchmark suite, is within a documented and
   honest fraction of Nginx for the scenarios we explicitly target.

That last point is deliberately not "faster than Nginx." Faster is a
nice-to-have. **Boring, correct, and operable** is the goal.
