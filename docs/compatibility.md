# Nginx Compatibility Matrix

Directive-by-directive view of "what works in Elrond today" versus "what
Nginx supports." This document is authoritative for **v0.37.0**.

- **✅ Implemented** — does what Nginx does, modulo any caveat noted.
- **🟡 Parsed-but-ignored** — accepted by the parser so real-world
  configs load; behavior not yet applied.
- **❌ Rejected** — recognized and refused with a clear error. We refuse
  rather than silently downgrade for anything safety-relevant.

Directives not in this list are *not* recognized; using them yields a
`line N: unknown directive 'X' in <ctx> context` error.

---

## Main context

| Directive                       | Status | Notes |
| ------------------------------- | :----: | ----- |
| `worker_processes`              |   🟡   | Stored; v0.x is single-process. |
| `pid`                           |   ✅   | Written at startup, removed on clean shutdown. |
| `error_log`                     |   ✅   | File output + `SIGUSR1` reopen. Falls back to stderr if absent. |
| `events { … }`                  |   🟡   | Block parsed; inner directives ignored. |
| `http { … }`                    |   ✅   | |
| `stream { … }`                  |   ✅   | TCP only; `listen … udp` is **❌ rejected**. |
| `include`                       |   ✅   | Relative-path resolution + cycle detection. |
| `user` / `daemon` / `master_process` | 🟡 | |

## `http` context

| Directive                              | Status | Notes |
| -------------------------------------- | :----: | ----- |
| `upstream { … }`                       |   ✅   | |
| `server { … }`                         |   ✅   | |
| `access_log` (path)                    |   ✅   | File output (target `access`). `SIGUSR1` reopens. |
| `proxy_cache_path`                     |   ✅   | `keys_zone=NAME:SIZE` honored; other args parsed and ignored (in-memory MVP). |
| `limit_req_zone`                       |   ✅   | |
| `limit_conn_zone`                      |   ✅   | |
| `map`                                  |   ✅   | Literal patterns only (no regex). Chained evaluation in declaration order. |
| `auth_request`                         |   ✅   | URL must be a full `http://…` (no internal-location subrequests yet). 2xx allows; otherwise the auth service's status is returned. |
| `mirror`                               |   ✅   | Fire-and-forget shadow GET; repeatable. Request bodies are not mirrored (only method + URL + selected headers). |
| `sendfile` / `tcp_nopush`              |   🟡   | |
| `keepalive_timeout`                    |   🟡   | |
| `types` / `default_type` / `types_hash_*` | 🟡 | Built-in MIME table is used. |
| `gzip` / `gzip_types`                  |   ✅   | Applies to static AND proxied responses (proxied size-guarded at 256 KiB). |
| `gzip_min_length`                      |   ✅   | Default 20 bytes (Nginx default). |
| `gzip_disable` / `gzip_comp_level` / `gzip_proxied` / `gzip_vary` / `gzip_buffers` | 🟡 | |
| `log_format`                           |   🟡   | Access line format is currently fixed. |
| `client_max_body_size`                 |   ✅   | Cascades to server level; enforced via `Content-Length` → `413`. |
| `server_tokens`                        |   🟡   | |
| `map_hash_*`                           |   🟡   | |

## `server` context

| Directive                              | Status | Notes |
| -------------------------------------- | :----: | ----- |
| `listen <port>` / `<host:port>`        |   ✅   | |
| `listen … ssl`                         |   ✅   | Requires `ssl_certificate` and `ssl_certificate_key`. |
| `listen … http2`                       |   ✅   | HTTP/2 is negotiated via ALPN automatically on TLS listeners. |
| `listen … default_server`              |   🟡   | First vhost on the listener is the default today. |
| `listen … udp`                         |   ❌   | Stream UDP not implemented. |
| `server_name`                          |   ✅   | Used for SNI multi-cert routing, `$server_name`, and `Host`-based vhost routing. |
| `root`                                 |   ✅   | Cascades into locations with no content directive. |
| `location <pat>`                       |   ✅   | Prefix, exact `=`, and regex (`~`, `~*`). `^~` accepted but treated as plain prefix (does not yet block regex consideration). |
| `ssl_certificate`                      |   ✅   | Multi-cert SNI: multiple `server` blocks on the same `listen` each declare their own cert. |
| `ssl_certificate_key`                  |   ✅   | |
| `ssl_protocols`                        |   ✅   | Tokens: `TLSv1.2`, `TLSv1.3`. The strictest set across vhosts wins per listener. Other tokens rejected. |
| `ssl_ciphers` / `ssl_prefer_server_ciphers` / `ssl_session_*` / `ssl_dhparam` / `ssl_ecdh_curve` / `ssl_stapling` | 🟡 | rustls defaults. |
| `gzip` / `gzip_types`                  |   ✅   | |
| `add_header N V`                       |   ✅   | Cascades into every location in this server; location-level wins on conflict. |
| `client_max_body_size`                 |   ✅   | |
| `error_log` / `index`                  |   🟡   | |
| `error_page`                           |   🟡   | |

## `location` context

| Directive                              | Status | Notes |
| -------------------------------------- | :----: | ----- |
| `return <status> [body]`               |   ✅   | Body supports variables. |
| `proxy_pass`                           |   ✅   | `http://…` or `https://…`; direct address, named upstream, or variable-driven template (e.g. `proxy_pass http://$pool;`). |
| `root`                                 |   ✅   | |
| `alias`                                |   ✅   | |
| `try_files`                            |   ✅   | Path-existence probes + `=NNN` final entry. Path-traversal-safe. No `@named` location targets yet. |
| `autoindex on\|off`                    |   ✅   | Directories first, dotfiles hidden. |
| `metrics`                              |   ✅   | Prometheus exposition format inline. |
| `proxy_set_header N V`                 |   ✅   | Templates rendered; empty value removes the header. |
| `add_header N V`                       |   ✅   | |
| `expires <duration>`                   |   ✅   | Sets `Cache-Control: max-age=N` and `Expires` HTTP-date. |
| `gzip on\|off`                         |   ✅   | Overrides server-level setting; applies to static AND proxied responses (size-guarded). |
| `auth_basic` / `auth_basic_user_file`  |   ✅   | **bcrypt-only** (`$2y$` / `$2a$` / `$2b$`); plain / APR1 / `{SHA}` rejected at config-load. |
| `allow X` / `deny X`                   |   ✅   | IPv4 / IPv6 / CIDR / `all`. First-match-wins. Empty rules → allow. |
| `limit_req zone=… [burst=N]`           |   ✅   | Token bucket; `nodelay` is implicit. Configurable `limit_req_status` ignored (always `503`). |
| `limit_conn zone=… N`                  |   ✅   | RAII guard; concurrent in-flight cap per key. |
| `proxy_cache <zone>`                   |   ✅   | |
| `proxy_cache_key <tpl>`                |   ✅   | |
| `proxy_cache_valid [code\|any]… <dur>` |   ✅   | Repeatable; safety guards reject caching `Set-Cookie` / `Vary` / `Cache-Control: no-store\|private\|no-cache` / body > 4 MiB. |
| `proxy_cache_bypass` / `proxy_no_cache` / `proxy_cache_lock` / `proxy_cache_use_stale` / `proxy_cache_revalidate` / `proxy_cache_methods` / `proxy_cache_min_uses` | 🟡 | |
| Cache `Vary` handling                  |   ✅   | Multiple variants per cache key, signed by the request's vary-axis headers. `Vary: *` → BYPASS (RFC 9111). |
| `proxy_ssl_verify on\|off`             |   ✅   | Default `on` (system trust store). `off` accepts any server cert — test/staging only. |
| `proxy_ssl_certificate` / `_key`       |   ✅   | mTLS to upstream. Both required; setting one alone is a config-load error. |
| `proxy_ssl_trusted_certificate` / `_server_name` / `_session_reuse` / `_protocols` / `_ciphers` | 🟡 | |
| `proxy_connect_timeout`                |   ✅   | Process-wide via `HttpConnector`. |
| `proxy_read_timeout`                   |   ✅   | Per-location; wraps the whole upstream exchange with `tokio::time::timeout`. |
| `proxy_send_timeout`                   |   🟡   | Covered in practice by `proxy_read_timeout`. |
| `proxy_next_upstream`                  |   🟡   | Hard-coded behavior: retry on connect-error and 5xx for idempotent methods (`GET`/`HEAD`/`OPTIONS`/`DELETE`). |
| `proxy_hide_header` / `proxy_pass_header` / `proxy_redirect` / `proxy_buffering` | 🟡 | |
| `index`                                |   🟡   | |

## `upstream` context

| Directive                              | Status | Notes |
| -------------------------------------- | :----: | ----- |
| `server <addr>`                        |   ✅   | |
| `weight=N`                             |   ✅   | |
| `max_fails=N` / `fail_timeout=Ns`      |   ✅   | Passive health. `ms` / `s` / `m` / `h` / `d` units. |
| `backup` / `down`                      |   ✅   | |
| `least_conn` / `ip_hash`               |   ✅   | |
| `health_check uri= interval= timeout= match=` | ✅ | Active probe; reuses passive health state machine. |
| `hash <var> [consistent]`              |   🟡   | Falls back to round-robin. |
| `keepalive` / `zone`                   |   🟡   | |

## `stream` context

| Directive                              | Status | Notes |
| -------------------------------------- | :----: | ----- |
| `upstream { … }`                       |   ✅   | |
| `server { listen, proxy_pass }`        |   ✅   | TCP only. |
| `listen … ssl`                         |   ❌   | TLS pass-through / termination not yet. |
| `listen … udp`                         |   ✅   | Stateless request/response UDP relay (DNS / syslog / metrics). 5s reply timeout. |
| `access_log` / `error_log` / `log_format` | 🟡 | |
| `proxy_timeout` / `proxy_connect_timeout` / `tcp_nodelay` | 🟡 | |
| `resolver`                             |   🟡   | |

## Variables

| Variable                  | Source |
| ------------------------- | ------ |
| `$host`                   | `Host` header → URI authority → `$server_name`. |
| `$server_name`            | From the matched vhost. |
| `$remote_addr`            | Client IP. |
| `$request_method`         | Method. |
| `$request_uri`            | Path + query (as received). |
| `$uri` / `$document_uri`  | Path only. |
| `$args` / `$query_string` | Query string. |
| `$scheme`                 | `https` on TLS listeners, `http` otherwise. |
| `$arg_<name>`             | Query argument (percent-decoded). |
| `$http_<name>`            | Request header (underscores → hyphens). |
| `$cookie_<name>`          | Cookie value. |
| `$<name>` declared by `map` | Resolved against the map; chained in declaration order. |
| `${unknown}`              | Renders as empty string. |

## Forwarded headers automatically set on proxied requests

| Header                   | Source |
| ------------------------ | ------ |
| `X-Real-IP`              | Client peer IP. |
| `X-Forwarded-For`        | Appends the client IP to any existing value. |
| `X-Forwarded-Proto`      | Listener scheme (`http` / `https`). |

## Signals (Unix)

| Signal               | Effect |
| -------------------- | ------ |
| `SIGINT` / `SIGTERM` | Graceful shutdown — stop accepting, drain in-flight requests. |
| `SIGHUP`             | Validate-then-swap config reload. TLS certs hot-reload. |
| `SIGUSR1`            | Reopen `access_log` / `error_log` files. |

## systemd

`Type=notify` integration: Elrond sends `READY=1` once listeners are up,
`RELOADING=1` at the start of a `SIGHUP` reload and `READY=1` when it
finishes, and `STOPPING=1` before exit. A reference unit file lives at
[`examples/elrond.service`](../examples/elrond.service).
