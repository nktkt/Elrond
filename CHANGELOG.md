# Changelog

All notable changes to Elrond are documented in this file.

## [0.39.0] - 2026-05-16

**Auto-`Alt-Svc` advertisement for HTTP/3.** 80 unit tests. Pre-alpha.

Browsers and HTTP-clients that arrive over TCP/TLS can now discover
the HTTP/3 endpoint and upgrade on a subsequent request, without the
operator having to write `add_header Alt-Svc …` themselves.

### Added

- A listener that has any vhost with `listen … http3;` now emits
  `Alt-Svc: h3=":<port>"; ma=86400` on every HTTPS (TCP/TLS)
  response. The port advertised is the listener's own TCP/UDP port
  (the QUIC endpoint binds the same number on UDP).
- Per-listener: any vhost opting into `http3` is enough — all vhosts
  on that address advertise Alt-Svc, matching how the QUIC endpoint
  itself spans the whole listener.
- Inserted before user-supplied `add_header`, so an explicit
  `add_header Alt-Svc "<custom>";` still overrides the auto value.

### Not advertised

- Plain HTTP listeners (no TLS) — browsers ignore `Alt-Svc` on
  non-secure origins, so emitting it would just be noise.
- TLS listeners that do **not** have HTTP/3 enabled — would point at
  a non-listening UDP port.
- The HTTP/3 path itself doesn't currently re-advertise (the client
  is already on h3); harmless, may be added later for very long-lived
  clients that want to refresh `ma`.

### Verified end-to-end

```
listen 8443 ssl http3;  →  alt-svc: h3=":8443"; ma=86400   ✓
listen 8444 ssl;        →  (no alt-svc)                    ✓
listen 8080;            →  (no alt-svc)                    ✓
curl --http3            →  HTTP/3 200, body "h3-enabled"   ✓
```

## [0.38.0] - 2026-05-16

**HTTP/3 over QUIC (Phase 11).** 80 unit tests. Pre-alpha.

Closes the single largest roadmap gap. HTTP/3 runs alongside the
regular TLS HTTP/1+2 listener on the same address: TCP keeps serving
HTTP/1.1 and HTTP/2; UDP serves HTTP/3 via QUIC. Same certificates,
same SNI multi-cert resolver, same routing, same actions.

### Added

- **`listen ... http3;`** (also `quic`) on a `server` block. Implies
  `ssl` — HTTP/3 only runs over QUIC, which is TLS 1.3 only.
- Adds `quinn = "0.11"`, `h3 = "0.0.8"`, `h3-quinn = "0.0.10"`.
- A separate `rustls::ServerConfig` per HTTP/3 listener with
  ALPN = `["h3"]` and TLS 1.3 only; shares the SNI multi-cert
  resolver with the TCP listener.
- **`src/http3.rs`** — `quinn::Endpoint` bound on the same UDP port
  as the TCP listener; per-QUIC-connection `h3::server::Connection`;
  per-stream task that:
  - Picks the right vhost via the existing `ListenerCfg::pick_state`.
  - Buffers the request body up to `client_max_body_size` (or a
    32 MiB absolute cap if "unlimited") — `413` mid-stream if it
    exceeds.
  - Runs the full pipeline: `allow`/`deny` → `limit_req` →
    `limit_conn` → `auth_request` → `auth_basic` → `mirror` →
    action.
  - Supports every action variant: `return`, `static`, `metrics`,
    `try_files`, **`proxy_pass`** (body buffered, forwarded through
    the same `proxy::forward` chain as HTTP/1+2).
- `proxy::forward` signature refactored to take `Request<ElrondBody>`
  so HTTP/1+2 and HTTP/3 share one code path.
- The access log distinguishes HTTP/3 entries with `(h3)`.

### Verified end-to-end

```nginx
server {
    listen 8443 ssl http3;
    server_name alpha.local;
    ssl_certificate     /path/to/alpha.crt;
    ssl_certificate_key /path/to/alpha.key;
    location / { return 200 "hello from h3\n"; }
}
```

Both `curl` (TLS HTTP/2) and `curl --http3` return `hello from h3`.
Access log shows `GET / 200` and `GET / 200 (h3)` from the same
listener. `lsof` confirms TCP + UDP on the same port.

### Known limitations

- No `Alt-Svc` advertisement.
- No HTTP/3 cert hot-reload on `SIGHUP` (TCP TLS still hot-reloads).
- No 0-RTT.
- Request bodies buffered up to the size cap; no streaming uploads.

## [0.37.0] - 2026-05-16

**`gzip_min_length` honored + `docs/compatibility.md` refresh.**
80 unit tests. Pre-alpha.

### Added

- **`gzip_min_length <N>;`** at server level. Default 20 bytes.
  Bodies shorter than N skip compression.

### Docs

- `docs/compatibility.md` refreshed to v0.37 baseline: regex
  `location`, `auth_request`, `mirror`, `https://` upstream,
  `proxy_ssl_verify`, `proxy_ssl_certificate` (mTLS),
  variable-driven `proxy_pass`, `listen … udp`, and Vary-aware
  cache are all now marked implemented.

## [0.36.0] - 2026-05-16

**mTLS to upstream — `proxy_ssl_certificate` / `_key`.** 80 unit tests.
Pre-alpha.

### Added

- **`proxy_ssl_certificate <path>;`** and
  **`proxy_ssl_certificate_key <path>;`** at location level. PEM-
  encoded cert chain and matching private key are loaded at config
  build, used to authenticate Elrond to the upstream over TLS
  (service-to-service mTLS).
- Combined with `proxy_ssl_verify`: client cert always presented; the
  flag still controls whether Elrond verifies the upstream's server
  cert. So all four combinations work — verify on or off, with or
  without client cert.
- **`proxy::ProxyClient`** — per-location HTTPS client (HTTP/1 + HTTP/2)
  built once with the right rustls `ClientConfig` and wrapped in an
  `Arc` on `LocationRt`. Reused for every request to that location.
- Configuration validation: setting only one of `proxy_ssl_certificate`
  / `proxy_ssl_certificate_key` is a hard config-load error pointing
  at the offending line.

### Verified

```nginx
location /mtls-required/ {
    proxy_pass https://example.com;
    proxy_ssl_certificate     /etc/elrond/clients/svc.crt;
    proxy_ssl_certificate_key /etc/elrond/clients/svc.key;
}
```

`elrond -t -c …` reports `config valid`. A config with only one of
the two paths fails with:

```
elrond: config error: line 4: location '/' must set both
'proxy_ssl_certificate' and 'proxy_ssl_certificate_key' (mTLS to
upstream) or neither
```

### Known follow-ups

- `proxy_ssl_trusted_certificate` (pin extra trust roots) parsed but
  not yet honored.
- `proxy_ssl_server_name` (override SNI sent to the upstream) parsed
  but not yet honored — SNI currently derives from the URL host.

## [0.35.0] - 2026-05-16

**`proxy_ssl_verify off;` — closes the v0.32 caveat.** 80 unit tests.
Pre-alpha.

The escape hatch for self-signed upstreams in test / staging. Default
remains **`on`** — we never silently downgrade.

### Added

- **`proxy_ssl_verify on|off;`** at location level. Default `on`.
- Two static HTTPS clients live in the proxy module:
  - `client()` — verifying client (system trust store).
  - `client_insecure()` — accepts any server cert via a custom
    `ServerCertVerifier` (gated behind `proxy_ssl_verify off;`).
- The per-location flag selects which client handles the request, on
  both the retry-safe and single-shot paths.
- Several `proxy_ssl_*` directives now parse without error
  (`proxy_ssl_certificate`, `_key`, `_trusted_certificate`,
  `_server_name`, `_session_reuse`, `_protocols`, `_ciphers`); mTLS
  enforcement comes in the next release.

### Verified end-to-end

Python HTTPS server on `127.0.0.1:7700` with a fresh self-signed cert.

| location              | result                       |
|-----------------------|------------------------------|
| default (verify on)   | `502` (cert chain rejected)  |
| `proxy_ssl_verify off`| `200 selfsigned-ok`          |

### Known follow-ups

- mTLS (`proxy_ssl_certificate` / `_key`) parsed but not honored.
- `proxy_ssl_trusted_certificate` (pin an extra trust root) parsed
  but not honored.

## [0.34.0] - 2026-05-16

**Cache: Vary-aware variants.** 80 unit tests. Pre-alpha.

Previously, any response with a `Vary` header was bypassed by the cache
(documented as a deliberate safety choice). v0.34.0 actually honors
`Vary`: one `proxy_cache_key` can now hold multiple variants, each keyed
by the request-header values the upstream said it varies on. The classic
case is `Vary: Accept-Encoding` — gzip and identity bodies are both
cached and served to the right client.

### Added

- **Vary-aware variants** keyed by a `vary_signature`
  (`name=value\0name=value\0…`). One `cache_key` → `Vec<Entry>`; lookup
  picks the variant whose signature matches the current request.
- **`Vary: *`** is still bypassed (RFC 9111 says it's uncacheable).
- Eviction operates per-variant, soonest-to-expire first.
- 3 new unit tests:
  - `vary_variants_kept_separately` — two requests with different
    `Accept-Encoding` get their own bodies.
  - `vary_request_without_matching_variant_misses` — a third encoding
    (`br`) misses because no `br` variant was stored.
  - `vary_star_is_bypassed`.

### Changed

- `cache::Entry` gains `vary_headers: Vec<String>` (lowercase, from the
  response) and `vary_signature: String` (computed from the request at
  store time).
- `cache::CacheStore::get(key, req_headers)` now takes the request's
  headers so it can select the right variant.
- `proxy::maybe_cache` reads the response's `Vary`, computes the
  signature from the captured request headers, and stores accordingly.

### Verified

```
PUT  k with Vary=accept-encoding, req gzip      → variant A "GZIPPED"
PUT  k with Vary=accept-encoding, req identity  → variant B "PLAIN"
GET  k with req gzip                            → A "GZIPPED"
GET  k with req identity                        → B "PLAIN"
GET  k with req br (no matching variant)        → MISS
```

### Known follow-ups

- No size cap per `cache_key` (a backend that varies on a high-cardinality
  header could grow many variants). Pair with `limit_req` if needed.
- `conditional revalidation` (304 from upstream still pending), and
  `stale-while-revalidate` not yet implemented.

## [0.33.0] - 2026-05-16

**UDP `stream` proxy.** 80 unit tests. Pre-alpha.

### Added

- **`listen <port> udp;`** in a `stream` `server` block. Stateless
  request/response relay: each client datagram is forwarded to a
  balancer-selected upstream, and the upstream's reply is sent back to
  the original client address.
- Per-exchange ephemeral upstream socket (no NAT-style session table)
  with a 5-second reply timeout. Suitable for DNS / syslog / metrics
  ingestion — long-lived UDP flows (QUIC, RTP) are out of scope.
- Reuses the existing `Balancer` / passive-health machinery (`ip_hash`,
  `max_fails` / `fail_timeout`, `backup`, `down` all apply); UDP packet
  counts feed the existing stream metrics
  (`elrond_stream_connections_accepted_total`,
  `_bytes_client_to_upstream_total`, `_bytes_upstream_to_client_total`,
  `_active_connections`).
- Listener log line now distinguishes `tcp://` vs `udp://`.

### Verified end-to-end

```nginx
stream {
    upstream dns_backend { server 127.0.0.1:19000; }
    server { listen 18000 udp; proxy_pass dns_backend; }
}
```

Against a Python UDP echo on `127.0.0.1:19000`:

```
sendto :18000 b"hello"  → reply b"echo:hello"
sendto :18000 b"world"  → reply b"echo:world"

/metrics →
  elrond_stream_connections_accepted_total      2
  elrond_stream_bytes_client_to_upstream_total 10
  elrond_stream_bytes_upstream_to_client_total 20
  elrond_stream_active_connections              0

log: stream listening on udp://0.0.0.0:18000
```

### Known limitations

- Stateless: each datagram is independent (no session affinity beyond
  `ip_hash`).
- No source-IP spoofing (the upstream sees the proxy's IP). Adding
  `proxy_bind` is a follow-up.
- No PROXY-protocol UDP encapsulation.

## [0.32.0] - 2026-05-16

**TLS upstream — `proxy_pass https://…`.** 80 unit tests. Pre-alpha.

### Added

- **`proxy_pass https://HOST:PORT;`** is now honored. Adds
  `hyper-rustls` and `rustls-native-certs` deps; the proxy client is
  now an `HttpsConnector<HttpConnector>` that auto-detects the URI
  scheme and does the TLS handshake against the upstream's cert,
  validated against the system trust store.
- `Balancer.scheme` field — `"http"` for named upstreams and
  `proxy_pass http://…`, `"https"` for `proxy_pass https://…`. The
  scheme threads through to the per-request URI we hand hyper.
- `https://` works for both fixed (`proxy_pass https://api.example.com`)
  and dynamic (`proxy_pass https://$pool`) targets.

### Verified end-to-end

```nginx
location / {
    proxy_pass https://example.com;
    proxy_set_header Host example.com;
}
```

Going through Elrond reaches the real Cloudflare-fronted
`example.com` over TLS:

```
$ curl -sI http://localhost:8080/
HTTP/1.1 200 OK
date: Sat, 16 May 2026 04:07:41 GMT
content-type: text/html
server: cloudflare
last-modified: …
```

The full HTML body comes through (`<!doctype html><html lang="en">…`).

### Known follow-ups

- **`proxy_ssl_verify off;`** parsed but not honored — server cert
  verification is always on against the system trust store. To trust
  a self-signed upstream cert today, install it in the host's trust
  store. Disable-flag plumbing is the next step.
- **mTLS upstream** (`proxy_ssl_certificate` / `..._key`) parsed but
  not honored. Adding per-location rustls `ClientConfig` overrides
  comes after `proxy_ssl_verify`.

## [0.31.0] - 2026-05-16

**`mirror` — fire-and-forget traffic shadowing.** 80 unit tests.
Pre-alpha.

### Added

- **`mirror <url>;`** at location level. Repeatable — every directive
  adds one shadow target. URLs are templates (variables rendered per
  request).
- For each matched request, Elrond spawns one task per mirror with a
  2-second timeout. Mirror responses are discarded. The original
  request's response, status, body, and timing are **never** affected.
- Mirrors receive `Authorization`, `Cookie`, `User-Agent` headers
  from the original request and an explicit `X-Elrond-Mirror: 1`
  marker so the shadow can distinguish replay traffic from real.
- Metrics: `elrond_mirror_attempts_total`, `elrond_mirror_failures_total`.

### Order

`allow`/`deny` → `limit_req` → `limit_conn` → **`mirror`** →
`auth_request` → `auth_basic` → action. Mirrors fire on every request
the client is allowed to make, regardless of upstream auth — useful
for shadow-traffic analytics where the auth service is itself the
shadow target.

### Verified end-to-end

```nginx
location / {
    mirror http://127.0.0.1:7400/shadow$request_uri;
    proxy_pass http://127.0.0.1:7300;
}
```

```
GET /first      → "production-resp"
GET /second     → "production-resp"
GET /api/three  → "production-resp"

shadow log:
  path=/shadow/first      ua=elrond-mirror mirror=1
  path=/shadow/second     ua=elrond-mirror mirror=1
  path=/shadow/api/three  ua=elrond-mirror mirror=1

/metrics: elrond_mirror_attempts_total 3, _failures_total 0
```

### Known limitations

- **Request bodies are not mirrored** (only method + URL + selected
  headers). Body replication would require buffering every request,
  which has its own correctness and memory costs; we'd rather not
  do it silently.
- Mirror failures don't affect the original response (by design) but
  they do show up in metrics so you can graph them.

## [0.30.0] - 2026-05-16

**`auth_request` — delegate authorization to an HTTP service.** 80 unit
tests. Pre-alpha.

### Added

- **`auth_request <url>;`** at location level. The URL is a template;
  per request, Elrond renders it and issues a `GET` to the rendered
  URL. `2xx` → the original request proceeds. Anything else → that
  status is returned to the client (with a short marker body so the
  auth service's internal response doesn't leak).
- Forwarded headers: `Authorization`, `Cookie`, `User-Agent`.
- Added headers: `X-Original-URI`, `X-Original-Method`, so the auth
  service knows what the user tried to do.
- Hard 5-second timeout on the subrequest; expiry → `504`. Connect /
  send errors → `500`.
- Ordering: `auth_request` runs **after** `allow`/`deny`,
  `limit_req`, and `limit_conn` (so denials don't pay the auth-service
  round-trip), and **before** `auth_basic` and the action.

### Verified end-to-end

```nginx
location /protected/ {
    auth_request http://127.0.0.1:7100/verify;
    proxy_pass   http://127.0.0.1:7200;
}
```

The auth service returns `200` iff `Authorization` contains
`good-token`, otherwise `403`.

| client request                          | result            |
|-----------------------------------------|-------------------|
| no `Authorization`                      | `403`             |
| `Authorization: Bearer bad-token`       | `403`             |
| `Authorization: Bearer good-token`      | `200 protected-data` |

Access log entries are tagged `(auth_request)` for denials.

### Known follow-ups

- No internal-location subrequest yet (URL must be a full
  `http://…`). Nginx supports `auth_request /local-path;`; we don't.
- The subrequest response body is discarded; passing through
  `Set-Cookie` from the auth service (Nginx
  `auth_request_set $var $upstream_http_x_user;` pattern) is a
  follow-up.

## [0.29.0] - 2026-05-16

**Regex `location` (`~` and `~*`).** 80 unit tests. Pre-alpha.

### Added

- **`location ~ pattern`** — case-sensitive regex match.
- **`location ~* pattern`** — case-insensitive regex match.
- **`location ^~ /prefix`** — accepted (treated as a plain prefix in
  v0.29.0; the "skip regex matching" semantics are documented as a
  follow-up).
- Adds the `regex` crate. Patterns are compiled at runtime build time
  with `regex::RegexBuilder`; bad patterns surface as
  `location regex 'XYZ' is invalid: …` at config-load.

### Routing precedence

Mirrors Nginx (with the v0.29 `^~` caveat noted above):

1. Exact match (`=`)
2. Regex match (`~` / `~*`) — first match in declaration order
3. Longest prefix

### Verified end-to-end

```nginx
location = /healthz { return 200 "exact"; }
location ~ "\.php$"    { return 200 "regex-php"; }
location ~* "\.JPEG?$" { return 200 "regex-jpeg-ci"; }
location /api/v1/ { return 200 "prefix-v1"; }
location /       { return 200 "prefix-root"; }
```

```
/healthz        → exact
/any/path.php   → regex-php
/image.JPEG     → regex-jpeg-ci    (case-insensitive)
/api/v1/users   → prefix-v1        (no regex matched; longest prefix)
/                → prefix-root
```

### Known follow-ups

- `^~` is accepted but treated as a plain prefix; full
  "if this prefix matches, skip regex consideration" semantics not
  yet wired.

## [0.28.0] - 2026-05-16

**Variable-driven `proxy_pass`.** 80 unit tests. Pre-alpha.

Completes the `map` → upstream pattern. Routing decisions made via
`map` / variables now flow straight into the proxy target — no more
duplicated location blocks per pool.

### Added

- **`proxy_pass http://$pool;`** (or any template with `$`) is now
  honored. At request time, Elrond renders the template, looks the
  result up in the named-upstream map first, and falls back to
  treating the rendered string as a direct address. Direct-address
  results are memoized so the same target reuses its in-flight /
  passive-health state across requests.
- `app::ProxyTarget` enum (`Fixed(Arc<Balancer>)` / `Dynamic { … }`)
  encapsulates the resolve-per-request logic.
- Unresolvable targets (empty string, no match) return a clear
  `502 Bad Gateway (empty / unresolvable proxy_pass)`.

### Verified end-to-end

```nginx
upstream premium  { server 127.0.0.1:7001; server 127.0.0.1:7002; }
upstream standard { server 127.0.0.1:7003; }

map $arg_plan $pool {
    "gold"    "premium";
    "silver"  "premium";
    default   "standard";
}

server {
    listen 8080;
    location / {
        proxy_pass http://$pool;
        add_header X-Pool $pool;
    }
}
```

```
?plan=gold   → premium-A premium-A premium-B premium-B   (RR within premium)
?plan=silver → premium-A premium-B                        (premium too)
(no plan)    → standard standard standard                 (default)
X-Pool       → premium                                    (header reflects routed pool)
```

### Known follow-ups

- Ephemeral balancers for direct-address dynamic targets currently
  have weight=1 / max_fails=1 / fail_timeout=10s defaults; not yet
  configurable.
- The ephemeral cache has no eviction; an attacker who can flood
  distinct `$pool` values can grow it unbounded. Pair with
  `limit_req` on a key the attacker controls.

## [0.27.0] - 2026-05-16

**Deployment-ready package: production config + systemd unit + logrotate
template + refreshed docs.** 80 unit tests. Pre-alpha (production-trial
ready).

This release is largely operational: no new directives, but the
artifacts needed to drop Elrond onto a host.

### Added

- **[`examples/production.conf`](examples/production.conf)** — a
  full-featured, production-shaped config:
  - HTTP → HTTPS redirect.
  - TLS multi-cert (`www`, `api`, `admin`) with `ssl_protocols`.
  - Cached static assets + SPA shell (`try_files`).
  - Proxied API with `proxy_read_timeout`, `proxy_cache`,
    `limit_req` (login burst protection), `limit_conn` (per-IP).
  - Admin area: `auth_basic` + `allow`/`deny` IP allow-list.
  - Loopback-bound `/metrics` endpoint behind `allow 127/8; deny all`.
  - Stream-block TCP proxy to PostgreSQL.
  - `map`-based variable derivation.
  - Security baseline headers (`Strict-Transport-Security`,
    `X-Frame-Options`, `X-Content-Type-Options`, `Referrer-Policy`).
- **[`examples/elrond.service`](examples/elrond.service)** —
  reference systemd unit. `Type=notify`, `ExecReload=/bin/kill -HUP`,
  `Restart=on-failure`, `LimitNOFILE=65536`, optional hardening lines
  commented out (uncomment after verifying in your env).
- **[`examples/logrotate.elrond`](examples/logrotate.elrond)** —
  daily rotation, 14-day retention, compress + delaycompress,
  `postrotate /bin/kill -USR1 …`. No `copytruncate` needed.

### Changed

- **`docs/compatibility.md`** rewritten to v0.27 baseline — every
  directive added through v0.26 is on the matrix.
- **README** "What works in v0.27.0" rewritten with the production
  trial pointer and the full capability list.
- **`elrond -t`** no longer touches the log files. This lets operators
  validate a config from a workstation without prepping
  `/var/log/elrond/`.
- Cleaned up several `dead_code` warnings on fields kept for
  diagnostics / future use.

### Verified

- `elrond -t` validates every shipped example config cleanly:
  `elrond.conf`, `examples/minimal.conf`, `examples/static.conf`,
  `examples/proxy.conf`, `examples/load_balance.conf`,
  `examples/v0_2_showcase.conf`.
- `examples/production.conf` parses end-to-end; running it just
  needs its referenced cert/key/htpasswd files to exist on the host
  (the error message tells you which one is missing).

## [0.26.0] - 2026-05-16

**gzip on proxied bodies + automatic `X-Forwarded-Proto`.** 80 unit
tests. Pre-alpha.

### Added

- **gzip on proxied responses.** Previously only static / `return` /
  `metrics` / `try_files` bodies were eligible — proxied responses
  streamed uncompressed. Now, when the location has `gzip on` and the
  upstream response advertises a `Content-Length` no larger than
  256 KiB (`PROXY_GZIP_MAX_COLLECT`), Elrond buffers and gzips.
  Larger or unknown-length bodies still stream through uncompressed
  (no memory blow-up).
- **`X-Forwarded-Proto`** is set automatically on every proxied
  request from the listener's scheme (`http` or `https`). This is what
  backends use to build absolute URLs and decide whether to set
  `Secure` cookies — getting it wrong silently is a class of common
  production bug.

### Changed

- `gzip::maybe_compress` gained a `max_collect: Option<usize>`
  argument. `None` (static path) means "no cap, body is already
  buffered". `Some(n)` (proxy path) means "skip if Content-Length is
  missing or > n".
- `add_forwarding_headers` now takes the scheme and sets
  `X-Forwarded-Proto`.

### Verified end-to-end

```
GET /headers       →  x-real-ip=127.0.0.1
                       x-forwarded-for=127.0.0.1
                       x-forwarded-proto=http
GET /              →  6041 bytes plain (Accept-Encoding: identity)
                   →   482 bytes gzipped on the wire (Accept-Encoding: gzip)
```

### Known follow-ups

- The 256 KiB threshold and the "skip when Content-Length is unknown"
  policy are hard-coded. Adding `gzip_min_length` / `gzip_buffers` /
  `gzip_proxied` enforcement is a follow-up.
- No Brotli (`br`).

## [0.25.0] - 2026-05-16

**Production limits & timeouts.** 80 unit tests. Pre-alpha.

The "no unbounded uploads, no hanging on a dead backend, no surprise
TLS 1.2 downgrades" baseline. With v0.23 (SNI multi-cert), v0.22 (file
logs / systemd), v0.19 (active health), and v0.5 (retry) this is the
minimum a service manager and an SRE actually need.

### Added

- **`client_max_body_size <size>;`** at server level.
  Accepts `k` / `m` / `g` suffixes; `0` = unlimited. Defaults to 1 MiB
  (`DEFAULT_CLIENT_MAX_BODY_SIZE`).
  Enforced at request entry by inspecting the `Content-Length` header:
  oversize requests get an immediate `413 Request Entity Too Large`,
  before any auth / proxy / cache work.
- **`proxy_connect_timeout <duration>;`** — applied to the upstream
  TCP connect via `HttpConnector::set_connect_timeout`. Defaults to
  10 s globally.
- **`proxy_read_timeout <duration>;`** at location level — caps the
  total upstream exchange (connect + send + read). On expiry, the
  attempt returns `502` and the peer is recorded as failed (so passive
  health kicks in). Defaults to 60 s.
- **`ssl_protocols TLSv1.2 TLSv1.3;`** at server level — restricts the
  TLS protocol versions offered. Tokens other than `TLSv1.2` /
  `TLSv1.3` are rejected at config-load. The strictest set across
  server blocks sharing a `listen` is used for that listener.
- `HttpConnector::set_nodelay(true)` on the proxy client — small win
  for proxied small responses.

### Verified end-to-end

| scenario                                | result            |
|-----------------------------------------|-------------------|
| `POST 500 B` (under 1KB limit)          | `200`             |
| `POST 2000 B` (over 1KB limit)          | `413`             |
| backend sleeps 5 s, `proxy_read_timeout 1s` | `502` in `~1.003 s` |
| `ssl_protocols TLSv1.3;` + TLS 1.2 client | handshake refused |
| same listener, TLS 1.3 client            | `Protocol: TLSv1.3` |

### Known follow-ups

- `client_max_body_size` is server-level only (no location override
  yet) and inspects `Content-Length`; chunked transfers without a
  length still consume up to the backend / cache buffering caps.
- `proxy_send_timeout` parsed but not yet wired (the unified
  `proxy_read_timeout` covers it in practice).
- `ssl_ciphers` / `ssl_ecdh_curve` / `ssl_session_*` still accepted
  but not applied.
- `proxy_connect_timeout` is a single process-wide setting on the
  HTTP client; per-location override comes when we move to per-pool
  clients.

## [0.24.0] - 2026-05-16

**`try_files` (Phase 3 closer).** 80 unit tests. Pre-alpha.

The standard SPA-hosting pattern works now:

```nginx
location / {
    try_files $uri /index.html;
}
```

### Added

- **`try_files arg1 arg2 … argN;`** at location level. Each non-final
  entry is treated as a path-existence probe rooted at the current
  `root`. The first existing file is served. The final entry is always
  honored:
  - **Path** (`/index.html`): served unconditionally (SPA fallback).
  - **Status** (`=404`, `=410`): returned as that status code; only
    valid as the last entry.
- Paths support full variable templates (`$uri`, `$arg_*`, etc.).
- Path-traversal protection: any rendered candidate containing `..`
  or other non-normal components is skipped (and `403`'d if it's the
  final entry).
- New `static_files::try_files()` helper that reuses the existing
  static serving pipeline (Range, ETag, conditional GET, HEAD) for
  whatever file ends up matched.
- gzip eligibility extended to `TryFiles` actions; SPA shells get
  compressed like any other static response.
- Config validation: `try_files` may share a location with `root` but
  not with `proxy_pass` / `return` / `metrics` / etc. — those produce
  a clear config-load error.

### Verified end-to-end

```
GET /assets/main.css          →  200  text/css       (real file)
GET /                         →  200  text/html      (root index)
GET /users/123/profile        →  200  text/html      (SPA fallback)
GET /api/unknown              →  404                  (=404)
GET /../../../etc/passwd      →  200  text/html      (SPA fallback;
                                                       not the host file)
```

### Known follow-ups

- No directory probe (`$uri/`) yet — a trailing slash is treated like
  any other path component.
- No `@named` location redirects; the final entry must be a Path or
  `=NNN`.

## [0.23.0] - 2026-05-16

**SNI multi-cert + per-`Host` virtual hosts on shared listeners.** 80
unit tests. Pre-alpha.

Until v0.23 a single TLS port could only host one hostname — the single
biggest production blocker. With this release, multiple `server` blocks
on the same `listen` address are collapsed into one listener:

- rustls picks the right certificate by SNI (`ResolvesServerCert`).
- HTTP routing picks the right `ServerState` by `Host` header.
- A `SIGHUP` reloads every certificate together via the existing TLS
  hot-reload pipeline.

### Added

- **Multi-cert TLS listener.** Configure several `server { listen 443
  ssl; server_name X; ssl_certificate …; ssl_certificate_key …; }`
  blocks on the same port; Elrond serves the correct cert per SNI and
  the correct vhost per `Host`.
- **`tls::CertEntry`** + **`tls::build_server_config(entries)`** — the
  multi-cert builder that wraps a custom `ResolvesServerCert` keyed by
  lowercase SNI name, with a default cert when no name matches.
- **`app::ListenerCfg`** — one entry per `listen` address, carrying a
  `Vec<VirtualHost>` and the TLS `ServerConfig` (if any). The first
  vhost is the default.
- **`Host`-header routing** at request entry (`server::handle_listener`).
  Port stripped, case-insensitive. Falls back to the first vhost when
  nothing matches.
- The listener startup log line now lists every vhost name:
  `listening on https://0.0.0.0:8443 (vhosts: alpha.local, beta.local)`.

### Changed

- `Runtime::servers` → `Runtime::listeners`. Each entry is now a
  `ListenerCfg` rather than a `(addr, state, tls)` tuple.
- `server::run` takes a `watch::Receiver<Arc<ListenerCfg>>` and routes
  per request.
- `supervisor` bins per addr; cert hot-reload rebuilds the entire
  SNI resolver in one shot, so adding / removing / renaming a vhost
  via `SIGHUP` does the right thing in one go.

### Refused early

- Mixing TLS and plain HTTP `server` blocks on the **same** `listen`
  address is now a config-load error rather than the previous "first
  one wins" silent footgun.

### Verified end-to-end

Two `server` blocks on `listen 8443 ssl;` with distinct certs
(`CN=alpha.local`, `CN=beta.local`):

| `openssl s_client -servername`     | Cert returned       |
|------------------------------------|---------------------|
| `alpha.local`                      | `CN=alpha.local` ✅ |
| `beta.local`                       | `CN=beta.local`  ✅ |
| `unknown.example`                  | `CN=alpha.local` (default) ✅ |

| `curl --resolve … https://…/`      | Body                              |
|------------------------------------|-----------------------------------|
| `alpha.local:8443`                 | `alpha-vhost from alpha.local:8443` |
| `beta.local:8443`                  | `beta-vhost from beta.local:8443`   |

### Known follow-ups

- `listen … default_server` flag not yet parsed; first vhost is always
  the default. Adding the flag is a small follow-up.
- No SNI-name validation against the cert's SAN list at config-load
  time. A cert mismatch surfaces at handshake time, not at startup.

## [0.22.0] - 2026-05-16

**Operational basics: file logs, `SIGUSR1` reopen, PID file, systemd
`Type=notify`.** 80 unit tests. Pre-alpha.

The minimum a service manager needs to host Elrond properly.

### Added

- **`access_log <path>;`** now writes to a file. Entries with
  `target: "access"` (one per served request) go here.
- **`error_log <path>;`** now writes to a file. Everything else
  (startup, reload, health, warnings, …) goes here.
- Falls back to **stdout / stderr** when the directives are absent —
  matches the existing behavior, so existing configs work unchanged.
- **`SIGUSR1`** reopens both files. The integration point for
  `logrotate` without `copytruncate`:
  ```text
  /var/log/elrond/*.log {
      daily
      rotate 14
      postrotate
          /bin/kill -USR1 $(cat /run/elrond.pid)
      endscript
  }
  ```
- **`pid <path>;`** actually writes the PID file at startup and removes
  it on clean shutdown.
- **systemd `Type=notify`** integration (Unix only): when
  `$NOTIFY_SOCKET` is set, Elrond sends
  - `READY=1` once all listeners are bound,
  - `RELOADING=1` at the start of a `SIGHUP` reload, `READY=1` when
    it finishes,
  - `STOPPING=1` on shutdown.
  Logs out a short `STATUS=…` string with the listener count.
- README's documentation block and `--help` updated to mention all
  three signals (`SIGHUP`, `SIGUSR1`, `SIGINT/SIGTERM`).

### Verified end-to-end

```
pid       /tmp/elog/elrond.pid;
error_log /tmp/elog/error.log;
http { access_log /tmp/elog/access.log; … }
```

- PID file present with the right value; removed on `SIGINT`.
- Two GETs → `access.log` has two access lines; `error.log` has the
  startup lines (no access lines mixed in).
- `mv access.log access.log.1 && kill -USR1` → the next request lands
  in a fresh `access.log` while the rotated file still has the old
  entries.

### Known follow-ups

- No `log_format` yet; the access line format is a fixed
  `INFO <client_ip> "<method> <path>" <status>`.
- systemd `Type=notify-reload` (newer protocol) not used; we send
  `RELOADING=1` / `READY=1` manually.

## [0.21.0] - 2026-05-15

**`map` directive (literal-pattern only).** 80 unit tests. Pre-alpha.

`map` is the most-requested missing variable derivation primitive. v0.21.0
ships a literal-only implementation with **chained evaluation**: a later
`map` can reference an earlier `map`'s output, which is by far the most
common use.

### Added

- **`map $source $output { … }`** at http level.
  - Patterns are literal strings: `"alpha" "value-1";`.
  - `default "fallback";` matches when nothing else does.
  - Result values are themselves templates — `"server-$tier"` works.
- **`$output`** is usable in every other template (`return`,
  `proxy_set_header`, `add_header`, `proxy_cache_key`, …).
- **Chained evaluation in declaration order.** A `map` can reference
  another `map` defined above it, so layered policy lookups (auth
  status → cache segment → backend selection) work naturally.
- `Unknown` variable references now consult the user-vars table; any
  template variable that isn't a built-in tries the map output first
  before rendering empty.

### Tests

- 80 unit tests (was 79). 1 new in `template::tests`:
  `user_var_resolves_through_template` — sets `$tier=gold` in the
  user-vars HashMap, renders `"hello $tier"`, asserts `"hello gold"`.

### Verified end-to-end

```nginx
map $arg_plan $tier {
    "gold"    "premium-tier";
    "silver"  "standard-tier";
    default   "free-tier";
}

map $tier $cache_segment {
    "premium-tier"  "premium";
    default         "shared";
}
```

```
GET /?plan=gold          → plan=gold     tier=premium-tier  segment=premium
GET /?plan=silver        → plan=silver   tier=standard-tier segment=shared
GET /?plan=missing-tier  → plan=missing  tier=free-tier     segment=shared
GET /                    → plan=         tier=free-tier     segment=shared
```

The chained lookup picked `segment=premium` only for `tier=premium-tier`,
confirming the later `map` could read the earlier `map`'s output.

### Known follow-ups

- No regex patterns (`~` / `~*`). Adding regex requires a regex crate
  decision and is its own release.
- No `volatile` / `hostnames` modifiers.
- Maps cannot recursively reference themselves; each map sees only
  outputs declared *above* it (Nginx-compatible behavior).

## [0.20.0] - 2026-05-15

**`allow` / `deny` (IP access control).** 79 unit tests. Pre-alpha.

The natural front for `/metrics`, `/private/`, admin paths, and any
`auth_basic` realm where you also want a network allow-list.

### Added

- **`allow <target>;`** and **`deny <target>;`** in location context.
  Targets:
  - `all`
  - A single IP: `192.0.2.1`, `::1`
  - A CIDR block: `10.0.0.0/8`, `2001:db8::/32`, `192.168.1.0/25`
- **First-match-wins** evaluation in declaration order. An empty rule
  list allows everyone (Nginx-compatible default).
- Denied requests get an immediate `403 Forbidden` and an access-log
  line tagged `(allow/deny)`.
- **Enforced before every other location-level check** (`limit_req`,
  `limit_conn`, `auth_basic`, the action), so blocked IPs do zero
  extra work.

### Tests

- 79 unit tests (was 71). 8 new in `access::tests`:
  - `all` target.
  - IPv4 / IPv6 single-address match.
  - IPv4 CIDR match including a partial-byte (`/25`) boundary.
  - IPv6 CIDR match.
  - First-match-wins ordering.
  - Empty-rules-allow.
  - Garbage / out-of-range input rejected.

### Verified end-to-end

```nginx
location = /metrics {
    allow 127.0.0.0/8;
    allow ::1;
    deny all;
    metrics;
}
```

- `curl http://127.0.0.1:8080/metrics` → `200`.
- `curl http://127.0.0.1:8080/private/` (with `allow 10.0.0.0/8;`,
  `allow 192.168.0.0/16;`, `deny all;`) → `403`.
- Access log: `127.0.0.1 "GET /private/" 403 (allow/deny)`.

## [0.19.0] - 2026-05-15

**Active upstream health checks** (Phase 5 closure). 71 unit tests.
Pre-alpha.

### Added

- **`health_check`** directive in `upstream` context. Options:
  - `uri=/path` (default `/`)
  - `interval=Ns` (default 5s)
  - `timeout=Ns` (default 2s)
  - `fails=N` / `passes=N` (reserved for richer transitions; today the
    existing `max_fails` / `fail_timeout` state machine handles the
    transitions)
  - `match=<status>` (default 200)
- **Background probe task per `upstream`** that GETs each non-`down`
  peer at the configured interval, with a timeout, and reports the
  outcome through `Peer::record_success` / `record_failure`. Active
  and passive health share the *same* health-state machine, so the
  failure cooldown and recovery behave the same regardless of which
  signal triggered them.
- **Task lifecycle.** The probe holds a `Weak<Balancer>` and exits on
  its next tick when the balancer is dropped — no JoinHandle
  bookkeeping required, reload-safe.

### Verified end-to-end

Two backends `A` (5001) and `B` (5002) with
`health_check uri=/health interval=500ms timeout=1s`:

```
baseline:                3× A, 3× B
touch /tmp/A.unhealthy:  0× A, 6× B   (probe took A out of rotation)
rm   /tmp/A.unhealthy:   3× A, 3× B   (probe brought A back in)
```

### Known follow-ups

- `fails` / `passes` are parsed but not yet wired into a richer
  multi-strike state machine — we lean on the existing
  `max_fails` / `fail_timeout` for now.
- No per-peer probe metrics yet (probe outcomes are reflected in the
  general passive-health counters but not split out).
- The probe uses a separate HTTP client from the proxy hot path —
  small extra memory, simpler isolation.

## [0.18.0] - 2026-05-15

**Concurrent-connection limiting (`limit_conn`).** 71 unit tests.
Pre-alpha.

The companion to `limit_req`: where rate caps requests-per-second,
`limit_conn` caps simultaneous in-flight per key. Together they cover
both bursty and long-tail abuse.

### Added

- **`limit_conn_zone <key> zone=NAME:SIZE;`** at http level. Same key-
  template / size-spec parsing as `limit_req_zone`.
- **`limit_conn zone=NAME N;`** (or `limit_conn NAME N;`) at location
  level — at most `N` simultaneous in-flight requests for any given
  key value.
- **RAII guard.** Acquiring a slot returns a `LimitConnGuard` held
  inside the request future; the counter is decremented automatically
  when the future drops, even if it panics or is cancelled.
- **Metrics:** `elrond_limit_conn_allowed_total`,
  `elrond_limit_conn_denied_total`.
- `limit_conn` is enforced **after** `limit_req` (rate first, count
  second), so a request that would already be denied for being too
  fast never takes a connection slot.

### Tests

- 71 unit tests (was 70). 1 new: `limit_conn_basic` — verifies
  acquire/deny/release semantics for one and two keys.

### Verified end-to-end

`limit_conn perip 2;` against a backend that sleeps 800 ms:

```
5 concurrent GET /slow/ → 2× 200, 3× 503
(after sleep)           → single GET → 200
/metrics                → allowed=3, denied=3
```

### Known follow-ups

- Configurable deny status (`limit_conn_status`) is accepted but
  ignored (always `503`).
- No per-zone metric labeling.
- No queueing — over-limit requests get `503` immediately.

## [0.17.0] - 2026-05-15

**Rate limiting (`limit_req`).** 70 unit tests. Pre-alpha.

The natural pairing with `auth_basic`: per-IP throttling protects login
endpoints from brute force, public APIs from runaway clients, and the
cache from one bad actor running it dry.

### Added

- **`limit_req_zone <key> zone=NAME:SIZE rate=Nr/s;`** at http level.
  `key` is a variable template (commonly `$remote_addr`, but
  `$arg_token` etc. work too). `SIZE` accepts `k` / `m` / `g` suffixes;
  Elrond translates bytes to an entry cap at ~64 bytes/entry.
- **`limit_req zone=NAME [burst=N];`** at location level. `nodelay` and
  `delay=N` are accepted for syntactic compatibility but the
  implementation is always "nodelay" today — over-budget requests get
  an immediate `503`.
- **Token-bucket implementation** (`src/limit.rs`): each key has a
  bucket of capacity `burst + 1` that refills at `rate` tokens/sec.
- **Eviction.** When the zone reaches `max_entries`, the
  least-recently-touched key is dropped on the next insertion, so a
  burst of one-off keys can't exhaust memory.
- `limit_req` is checked **before** `auth_basic`, so a request the
  rate-limit denies never reaches password verification — important
  for keeping bcrypt CPU low during a brute-force attempt.
- **Metrics:** `elrond_limit_req_allowed_total`,
  `elrond_limit_req_denied_total`.

### Tests

- 70 unit tests (was 64). 6 new in `limit::tests`:
  - `allows_first_burst_immediately`
  - `denies_after_burst_drained`
  - `separate_keys_have_separate_buckets`
  - `refill_replenishes_tokens` (sleeps 20 ms, verifies refill)
  - `parses_rate_specs` (`5r/s`, `60r/m`, malformed input)
  - `eviction_caps_entries`

### Verified end-to-end

Config: `limit_req_zone $remote_addr zone=api:10m rate=5r/s; … limit_req
zone=api burst=3;` — capacity = burst + 1 = 4.

```
GET /api/ × 10 →  200 200 200 200 503 503 503 503 503 503
/metrics    →  elrond_limit_req_allowed_total 4
                elrond_limit_req_denied_total 6
```

### Known follow-ups

- No `limit_conn` (concurrent-connection limit) yet — the obvious next
  small feature.
- `nodelay` is the only mode; queued/delayed shaping not yet.
- Configurable deny status (`limit_req_status`) accepted but ignored
  (always `503`).
- No per-zone metrics labeling — counters are global.

## [0.16.0] - 2026-05-15

**HTTP Basic auth (`auth_basic`).** 64 unit tests. Pre-alpha.

### Added

- **`auth_basic <realm>;`** at location level. `auth_basic off;` disables.
- **`auth_basic_user_file <path>;`** — htpasswd-style file.
- **Bcrypt-only** password hashes. Plain-text passwords, Apache APR1
  (`$apr1$…`), and the SHA-1 variant `{SHA}…` are **rejected at
  configuration-load time** with a line-numbered error that suggests
  `htpasswd -nbB user password`. We refuse weak crypto rather than
  silently accepting it.
- `WWW-Authenticate: Basic realm="<realm>"` on every `401`, with the
  realm string properly quoted.
- A clear `400`/`401` separation: bad base64, wrong scheme, missing
  user, and wrong password all return `401` with the same challenge.
- New deps: `bcrypt = "0.16"` and `base64 = "0.22"`.

### Tests

- 64 unit tests (was 60). 4 new in `auth::tests`:
  - Plain-text entries rejected with a clear message.
  - APR1 entries rejected.
  - Bcrypt hash loaded and verified (using a freshly minted cost-4 hash
    for "swordfish").
  - Empty / comment-only files rejected.

### Verified end-to-end

- Public location: `200`.
- Private location, no credentials → `401` + `WWW-Authenticate: Basic
  realm="private area"`.
- Wrong password → `401`.
- Correct password (`-u alice:swordfish`) → `200`.
- Plaintext htpasswd at startup:
  `config error: /tmp/bad.htpasswd:1: user 'alice' uses a non-bcrypt
  hash. Elrond only accepts bcrypt (\`$2y$…\`, \`$2a$…\`, \`$2b$…\`).
  Re-create with \`htpasswd -nbB user password\`.`

### Known follow-ups

- `auth_basic` is enforced at location level only — no server-level
  cascade yet. Add to every location that needs it.
- Brute-force protection is the deployer's responsibility today. A
  per-IP `limit_req` is the right pairing once that directive lands.
- No `satisfy any|all` semantics (Nginx combines `auth_basic` with
  `allow/deny`); we don't have `allow/deny` yet.

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

[0.39.0]: https://github.com/nktkt/Elrond/releases/tag/v0.39.0
[0.38.0]: https://github.com/nktkt/Elrond/releases/tag/v0.38.0
[0.37.0]: https://github.com/nktkt/Elrond/releases/tag/v0.37.0
[0.36.0]: https://github.com/nktkt/Elrond/releases/tag/v0.36.0
[0.35.0]: https://github.com/nktkt/Elrond/releases/tag/v0.35.0
[0.34.0]: https://github.com/nktkt/Elrond/releases/tag/v0.34.0
[0.33.0]: https://github.com/nktkt/Elrond/releases/tag/v0.33.0
[0.32.0]: https://github.com/nktkt/Elrond/releases/tag/v0.32.0
[0.31.0]: https://github.com/nktkt/Elrond/releases/tag/v0.31.0
[0.30.0]: https://github.com/nktkt/Elrond/releases/tag/v0.30.0
[0.29.0]: https://github.com/nktkt/Elrond/releases/tag/v0.29.0
[0.28.0]: https://github.com/nktkt/Elrond/releases/tag/v0.28.0
[0.27.0]: https://github.com/nktkt/Elrond/releases/tag/v0.27.0
[0.26.0]: https://github.com/nktkt/Elrond/releases/tag/v0.26.0
[0.25.0]: https://github.com/nktkt/Elrond/releases/tag/v0.25.0
[0.24.0]: https://github.com/nktkt/Elrond/releases/tag/v0.24.0
[0.23.0]: https://github.com/nktkt/Elrond/releases/tag/v0.23.0
[0.22.0]: https://github.com/nktkt/Elrond/releases/tag/v0.22.0
[0.21.0]: https://github.com/nktkt/Elrond/releases/tag/v0.21.0
[0.20.0]: https://github.com/nktkt/Elrond/releases/tag/v0.20.0
[0.19.0]: https://github.com/nktkt/Elrond/releases/tag/v0.19.0
[0.18.0]: https://github.com/nktkt/Elrond/releases/tag/v0.18.0
[0.17.0]: https://github.com/nktkt/Elrond/releases/tag/v0.17.0
[0.16.0]: https://github.com/nktkt/Elrond/releases/tag/v0.16.0
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
