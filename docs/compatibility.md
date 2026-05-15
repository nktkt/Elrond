# Nginx Compatibility Matrix

This is the directive-by-directive view of "what works in Elrond today"
versus "what Nginx supports." It corresponds to Elrond v0.12.0.

Three statuses are tracked:

- **✅ Implemented** — the directive does what Nginx does, modulo any
  caveats listed.
- **🟡 Parsed-but-ignored** — the directive is accepted by the parser so
  that real-world configs load, but the behavior is not yet applied.
- **❌ Rejected** — the directive is recognized and refused with a clear
  error. We refuse rather than silently downgrade for anything safety-
  relevant.

Directives not in this list are *not* recognized today; using them yields
a `line N: unknown directive 'X' in <ctx> context` error.

---

## Main context

| Directive                  | Status | Notes                              |
| -------------------------- | ------ | ---------------------------------- |
| `worker_processes`         | 🟡     | Stored for reporting; single-process today. |
| `pid`                      | 🟡     | Parsed; not yet written.           |
| `error_log`                | 🟡     | Stored; logging still goes to stdout via `tracing`. |
| `events { … }`             | 🟡     | Block parsed; inner directives ignored. |
| `http { … }`               | ✅     |                                    |
| `stream { … }`             | ✅     | TCP only; UDP rejected.            |
| `include`                  | ✅     | Relative-path resolution + cycle detection. |
| `user`                     | 🟡     | Accepted for forward compatibility. |
| `daemon` / `master_process`| 🟡     | Accepted; Elrond runs in the foreground. |

## `http` context

| Directive                  | Status | Notes                              |
| -------------------------- | ------ | ---------------------------------- |
| `upstream { … }`           | ✅     | See "Upstream context".            |
| `server { … }`             | ✅     |                                    |
| `access_log` (path)        | 🟡     | Stored; access goes to stdout via `tracing`. |
| `proxy_cache_path`         | ✅     | `keys_zone=NAME:SIZE` honored; other args parsed and ignored. |
| `sendfile` / `tcp_nopush`  | 🟡     |                                    |
| `keepalive_timeout`        | 🟡     |                                    |
| `types_hash_*` / `types`   | 🟡     | Built-in MIME table is used.       |
| `default_type`             | 🟡     | Falls back to `application/octet-stream`. |
| `gzip` / `gzip_types`      | ✅     | See "gzip" below.                  |
| `gzip_min_length`          | 🟡     | Hardcoded at 20 bytes (Nginx default). |
| `gzip_disable` / `gzip_comp_level` / `gzip_proxied` / `gzip_vary` / `gzip_buffers` | 🟡 | Accepted; not yet applied. |
| `log_format`               | 🟡     |                                    |
| `client_max_body_size`     | 🟡     |                                    |
| `server_tokens`            | 🟡     |                                    |
| `map_hash_*`               | 🟡     |                                    |

## `server` context

| Directive                  | Status | Notes                              |
| -------------------------- | ------ | ---------------------------------- |
| `listen <port>`            | ✅     |                                    |
| `listen <host:port>`       | ✅     |                                    |
| `listen … ssl`             | ✅     | Requires `ssl_certificate` and `ssl_certificate_key`. |
| `listen … http2`           | ✅     | HTTP/2 is negotiated via ALPN on TLS listeners automatically. |
| `server_name`              | ✅     | Used for `$server_name` and routing diagnostics; SNI multi-cert still pending. |
| `root`                     | ✅     | Cascades into locations with no content directive. |
| `location <pat>`           | ✅     | Exact `=`, prefix; `~` / `~*` / `^~` rejected. |
| `ssl_certificate`          | ✅     |                                    |
| `ssl_certificate_key`      | ✅     |                                    |
| `ssl_protocols` etc.       | 🟡     | rustls defaults; tuning ignored.   |
| `gzip` / `gzip_types`      | ✅     |                                    |
| `error_log`                | 🟡     |                                    |
| `client_max_body_size`     | 🟡     |                                    |
| `error_page`               | 🟡     |                                    |
| `index`                    | 🟡     | Static serving uses `index.html`.  |

## `location` context

| Directive                  | Status | Notes                              |
| -------------------------- | ------ | ---------------------------------- |
| `return <status> [body]`   | ✅     | Body supports variables.           |
| `proxy_pass`               | ✅     | Direct address or named upstream.  |
| `root`                     | ✅     |                                    |
| `alias`                    | ✅     |                                    |
| `metrics`                  | ✅     | Renders Prometheus format inline.  |
| `proxy_set_header N V`     | ✅     | Empty rendered value removes the header. |
| `add_header N V`           | ✅     |                                    |
| `expires <dur>`            | ✅     | Sets `Cache-Control` and `Expires`. |
| `gzip on\|off`             | ✅     | Overrides server-level setting.    |
| `proxy_cache <zone>`       | ✅     |                                    |
| `proxy_cache_key <tpl>`    | ✅     |                                    |
| `proxy_cache_valid …`      | ✅     | Repeatable; default key is `$scheme$host$request_uri`. |
| `proxy_buffering` / `proxy_*_timeout` | 🟡 | Accepted; v0.x uses hyper defaults. |
| `proxy_next_upstream`      | 🟡     | Retry on connect-error and 5xx is the hard-coded behavior for now. |
| `proxy_hide_header` / `proxy_pass_header` / `proxy_redirect` | 🟡 |   |
| `proxy_cache_bypass` / `proxy_no_cache` / `proxy_cache_lock` / `proxy_cache_use_stale` / `proxy_cache_revalidate` / `proxy_cache_methods` / `proxy_cache_min_uses` | 🟡 | |
| `index`                    | 🟡     |                                    |
| `try_files`                | 🟡     |                                    |
| `autoindex`                | 🟡     |                                    |
| `add_header` (server-level)| 🟡     | Only location-level applies today. |

## `upstream` context

| Directive                  | Status | Notes                              |
| -------------------------- | ------ | ---------------------------------- |
| `server <addr>`            | ✅     |                                    |
| `weight=N`                 | ✅     |                                    |
| `max_fails=N`              | ✅     |                                    |
| `fail_timeout=Ns`          | ✅     | `ms`, `s`, `m`, `h`, `d` units.    |
| `backup`                   | ✅     |                                    |
| `down`                     | ✅     |                                    |
| `least_conn`               | ✅     |                                    |
| `ip_hash`                  | ✅     |                                    |
| `hash <var> [consistent]`  | 🟡     | Falls back to round-robin.         |
| `keepalive`                | 🟡     |                                    |
| `zone`                     | 🟡     |                                    |
| Active health checks       | ❌     | Not implemented (Nginx Plus only). |

## `stream` context

| Directive                  | Status | Notes                              |
| -------------------------- | ------ | ---------------------------------- |
| `upstream { … }`           | ✅     | Same syntax as HTTP upstreams.     |
| `server { listen, proxy_pass }` | ✅ | TCP only.                          |
| `listen … ssl`             | ❌     | TLS pass-through / termination not yet. |
| `listen … udp`             | ❌     | UDP not yet.                       |
| `access_log` / `error_log` / `log_format` | 🟡 |                                  |
| `proxy_timeout` / `proxy_connect_timeout` / `tcp_nodelay` | 🟡 |                                  |
| `resolver`                 | 🟡     |                                    |

## Variables

The variable engine resolves these names (and `${braced}` forms):

| Variable                  | Source                                     |
| ------------------------- | ------------------------------------------ |
| `$host`                   | `Host` header, falling back to `server_name`. |
| `$server_name`            | The `server_name` directive.               |
| `$remote_addr`            | Client IP.                                 |
| `$request_method`         | Method.                                    |
| `$request_uri`            | Path + query (as received).                |
| `$uri` / `$document_uri`  | Path only.                                 |
| `$args` / `$query_string` | Query string.                              |
| `$scheme`                 | `http` (TLS termination uses `http` for now; this will become `https` for TLS listeners in a later release). |
| `$arg_<name>`             | Query argument by name (percent-decoded).  |
| `$http_<name>`            | Request header by name (underscores → hyphens). |
| `$cookie_<name>`          | Cookie value.                              |
| `${unknown}`              | Renders as empty string.                   |
