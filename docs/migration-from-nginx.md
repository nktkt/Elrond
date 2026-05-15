# Migrating from Nginx to Elrond

This guide walks through dropping Elrond in front of an application that
is currently fronted by Nginx. The point is to show what carries over
verbatim, what needs a small edit, and what isn't supported yet.

If you're evaluating whether Elrond can take over your current Nginx role
at all, read [`compatibility.md`](compatibility.md) first — that's the
authoritative directive matrix. This document is the *procedure*.

---

## 1. Decide whether Elrond can host your workload

Elrond's pre-alpha (v0.12.0) sweet spot is:

- **HTTP/1.1 and HTTP/2 reverse proxy** for a single-cert TLS frontend.
- **Static file serving** with `root`, `alias`, range requests, and gzip.
- **Round-robin / least-conn / ip-hash load balancing** with passive
  health.
- **TCP `stream` proxying** for a database / Redis / opaque backend.
- **An in-memory cache** for cacheable GET responses.

Elrond is **not** yet a fit if you depend on any of:

- HTTP/3 / QUIC.
- Multiple TLS certificates served by one listener via SNI.
- TLS cert hot reload without restart.
- Active health checks (Nginx Plus' `health_check`).
- `auth_request`, `auth_basic`, `limit_req`, `limit_conn`, `mirror`,
  named-location fallback in `try_files`.
- Regex `location` patterns (`~`, `~*`, `^~`).
- The full `rewrite` directive family.
- `map` / `geo` for variable derivation.
- A formal cache-poisoning test suite gating production.
- `mail { … }` (mail proxy).

If any of those are showstoppers, watch the
[roadmap](../ROADMAP.md) and the [changelog](../CHANGELOG.md) for the
phase that lands them.

---

## 2. Take stock of your current Nginx config

Most non-trivial Nginx configs share a structure:

```nginx
worker_processes auto;
events { worker_connections 4096; }

http {
    include      mime.types;
    default_type application/octet-stream;
    sendfile     on;
    gzip         on;

    log_format  main  '$remote_addr - $remote_user [$time_local] ...';
    access_log  /var/log/nginx/access.log  main;

    upstream app {
        server 10.0.0.1:3000;
        server 10.0.0.2:3000;
    }

    server {
        listen 80;
        listen 443 ssl http2;
        server_name www.example.com;

        ssl_certificate     /etc/nginx/cert.pem;
        ssl_certificate_key /etc/nginx/key.pem;

        location /static/ {
            root /var/www;
            expires 1d;
        }

        location / {
            proxy_pass http://app;
            proxy_set_header Host       $host;
            proxy_set_header X-Real-IP  $remote_addr;
            proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        }
    }
}
```

Almost every line of that lands cleanly in Elrond. The ones that need
attention:

| Original Nginx                               | Action for Elrond v0.12.0                    |
| -------------------------------------------- | -------------------------------------------- |
| `include mime.types;`                        | Keep. We tolerate `types { … }`.             |
| `default_type application/octet-stream;`     | Tolerated; our built-in table is used.       |
| `sendfile on; tcp_nopush on;`                | Tolerated; we use the runtime's I/O.         |
| `log_format main '…'; access_log /path main;`| Tolerated; logs currently go to stdout via `tracing`. Pipe stdout to a log file from your service manager. |
| `listen 443 ssl http2;`                      | `listen 443 ssl;` — HTTP/2 is negotiated automatically via ALPN once TLS is configured. |
| `proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;` | Drop this line. Elrond appends the client IP to the existing `X-Forwarded-For` header automatically. |
| Multiple `server { … }` blocks sharing one cert | Works today only if each has its own `listen` port. Multi-cert SNI is on the roadmap. |
| `location ~ \\.php$ { … }` (regex)           | Not supported yet. Either replace with prefix matching or wait for the regex-location release. |
| `try_files $uri $uri/ /index.html;`          | Tolerated but no-op. For a single-page-app fallback, use a `location / { return 200 …; }` or an explicit `index.html`. |
| `if ($request_method = POST) { return 405; }`| Not supported. The Nginx `if` quirk has a well-known footgun, and we'd rather not reproduce it. |
| `client_max_body_size 50m;`                  | Tolerated; the limit is hyper's default for now. |

---

## 3. Migrate

The cleanest approach is to write a parallel Elrond config and bring it
up on a different port, validating end-to-end before flipping DNS or the
load balancer in front of you.

### Validate the config before launching

```sh
elrond -t -c elrond.conf
```

`elrond -t` exercises the same load + build pipeline as a real start. It
catches everything from unknown directives to "your cert file is
empty" — at the same line numbers your editor reports.

### Run on a sibling port

```nginx
http {
    server {
        listen 8080;          # ← live Nginx still owns :80/443
        server_name www.example.com;
        # …everything else…
    }
}
```

Hit `http://your-host:8080/...` with your real traffic patterns. Look
at `curl -I` to verify response headers, and `curl --compressed` to
confirm gzip. Scrape `/metrics` to see request rates and proxy
attempt/failure counters.

### Cut over

There is nothing Elrond-specific about cutting over. Switch the upstream
DNS record, switch the load balancer target, or hot-restart a service
manager that bound the ports in advance — whichever you already use for
zero-downtime Nginx releases.

### Roll back

Elrond does not require any persistent state outside the cache (which
restarts cold). Rolling back to Nginx is as simple as stopping Elrond
and starting Nginx on the same ports.

---

## 4. After the cut-over

Worth doing in the first 24 hours:

- Scrape `/metrics` into Prometheus and graph
  `elrond_responses_total`, `elrond_proxy_failures_total`,
  `elrond_cache_hits_total`, and `elrond_active_connections`.
- Run `kill -HUP $(pidof elrond)` on a no-op config edit and confirm
  the access log shows no dropped requests.
- If you serve any `Set-Cookie` responses, confirm they're correctly
  flagged as `X-Cache: BYPASS` so you don't accidentally cache them.
- If you use `least_conn` or `ip_hash`, watch the per-upstream metrics
  (coming soon as labeled counters; for now, check `/metrics` snapshots
  on a 30-second cadence).

---

## 5. Known frictions, and where they go

- **Logging to a file.** Today Elrond writes to stdout via `tracing`.
  Pipe stdout to `logrotate` or your service manager. A future release
  will honor `access_log /path;` and reopen on `SIGUSR1`.
- **TLS cert rotation.** Today restart on rotation. Cert hot-reload on
  `SIGHUP` is a documented roadmap item.
- **Per-server `add_header`.** Today only `location`-level `add_header`
  applies. If you use server- or http-level `add_header` for things like
  `Strict-Transport-Security`, duplicate them into each location for now.
- **`X-Cache` header.** New compared to Nginx; downstream caches and
  proxies might log it. If that's noisy, file an issue — we can gate it
  behind a directive.

---

## 6. When something goes wrong

- `ELROND_LOG=debug` for verbose request-path logging.
- `elrond -t -c elrond.conf` to validate config without binding ports.
- `/metrics` for steady-state counters.
- Open an issue with: the offending config snippet, the request, and the
  response Elrond gave.

The repo lives at https://github.com/nktkt/Elrond.
