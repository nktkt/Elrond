# Changelog

All notable changes to Elrond are documented in this file.

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

[0.5.0]: https://github.com/nktkt/Elrond/releases/tag/v0.5.0
[0.4.0]: https://github.com/nktkt/Elrond/releases/tag/v0.4.0
[0.3.0]: https://github.com/nktkt/Elrond/releases/tag/v0.3.0
[0.2.0]: https://github.com/nktkt/Elrond/releases/tag/v0.2.0
[0.1.0]: https://github.com/nktkt/Elrond/releases/tag/v0.1.0
