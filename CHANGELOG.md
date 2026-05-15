# Changelog

All notable changes to Elrond are documented in this file.

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

[0.2.0]: https://github.com/nktkt/Elrond/releases/tag/v0.2.0
[0.1.0]: https://github.com/nktkt/Elrond/releases/tag/v0.1.0
