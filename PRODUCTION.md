# Elrond — Production Runbook

This is the **operator-facing** guide: what to set up, what to watch,
what to do when something breaks. For the directive reference, see
[`README.md`](README.md) and [`docs/compatibility.md`](docs/compatibility.md).

> **Status.** Pre-alpha. Elrond ships TLS 1.2/1.3, HTTP/1.1, HTTP/2,
> HTTP/3 (auto-`Alt-Svc`), load balancing, caching, ACLs, rate limits,
> mTLS to upstreams, and graceful reload. It does **not** yet ship
> ACME, OCSP stapling, OpenTelemetry, on-disk cache persistence,
> or `stale-while-revalidate`. If you need those today, stay on Nginx.

---

## 1. Deployment shapes

Three supported deployment shapes, from least to most "real":

| Shape | Use when |
|---|---|
| **Bare binary + systemd** | One host, hand-rolled OS (Debian, RHEL, Arch). |
| **Container (Docker / Kubernetes)** | Anything where the rest of the stack is containerized. |
| **`cargo install elrond`** | Dev / staging only. Do not ship to production. |

### 1.1 Systemd (bare binary)

```bash
# Build / install
cargo build --release
sudo install -m 0755 target/release/elrond /usr/local/sbin/elrond

# Drop the unit and example config into place
sudo install -m 0644 examples/elrond.service /etc/systemd/system/
sudo install -m 0644 examples/production.conf /etc/elrond/elrond.conf
sudo install -m 0644 examples/logrotate.elrond /etc/logrotate.d/elrond

# An unprivileged user to drop to
sudo useradd --system --no-create-home --shell /usr/sbin/nologin elrond

# Log + cert dirs
sudo install -d -m 0755 -o elrond -g elrond /var/log/elrond
sudo install -d -m 0750 -o root   -g elrond /etc/elrond/certs

# Validate, enable, start
sudo elrond -t -c /etc/elrond/elrond.conf
sudo systemctl daemon-reload
sudo systemctl enable --now elrond
sudo systemctl status elrond
```

The unit file uses `Type=notify`, so systemd only marks Elrond active
after the `READY=1` notification fires (i.e. after every listener is
bound, every TLS cert is loaded, every backend is reachable for the
first round of health checks).

### 1.2 Docker / GHCR

```bash
# Pull
docker pull ghcr.io/nktkt/elrond:latest

# Run with your own config + certs
docker run -d --name elrond \
  -p 80:8080 -p 443:8443 -p 443:8443/udp \
  -v $PWD/elrond.conf:/etc/elrond/elrond.conf:ro \
  -v $PWD/certs:/etc/elrond/certs:ro \
  --restart unless-stopped \
  ghcr.io/nktkt/elrond:latest
```

The image runs as root by default (so the in-image `user elrond elrond;`
in the default config can do its drop dance after binding). If you
prefer to run rootless and don't need privileged ports, override:

```bash
docker run --user elrond:elrond \
  -p 8080:8080 -v $PWD/elrond.conf:/etc/elrond/elrond.conf:ro \
  ghcr.io/nktkt/elrond:latest
```

…and remove the `user` directive from your config.

---

## 2. Privilege model

| Mode | Bind `:80`/`:443`? | Run as root? |
|---|---|---|
| `user elrond elrond;` + start as root | ✅ | No — drops after bind |
| `CAP_NET_BIND_SERVICE` (systemd `AmbientCapabilities=`) | ✅ | No — never had it |
| Run unprivileged, no `user` | Only if port ≥ 1024 | No |
| Run as root, no `user` directive | ✅ | **Yes — refuses silently, warns loudly** |

> **Elrond will warn** at startup if it's running as root and `user`
> is not set. It will **fail to start** if `user` is set but it can't
> drop (e.g. the process started as the wrong identity). Both behaviors
> are deliberate — silently keeping root privileges is the worst
> failure mode and we refuse to do it.

The bind-then-drop ordering is hard-wired:

1. Parse + validate config.
2. Install logging (so the rest of startup is loggable).
3. Raise `RLIMIT_NOFILE` if requested (root-only for the *hard* cap).
4. Write the PID file.
5. **Bind every listener** — TCP, TLS+TCP, QUIC/UDP.
6. **Drop privileges** (`initgroups` → `setgid` → `setuid`).
7. Announce readiness (`READY=1` to systemd).
8. Begin serving.

---

## 3. Reload semantics

| Signal | Effect | Survives a bad config? |
|---|---|---|
| `SIGHUP` | Re-read the config, hot-swap listeners and TLS certs. | ✅ Bad config → keep running on the old one, log the error. |
| `SIGUSR1` | Reopen `access_log` / `error_log` (for logrotate). | n/a |
| `SIGTERM` / `SIGINT` | Graceful drain, then exit. | n/a |

**Cert rotation.** Replace the cert files on disk, send `SIGHUP`.
The TLS listener swaps in the new cert without dropping in-flight
connections. ⚠ HTTP/3 (QUIC) currently does **not** hot-swap certs —
restart the process for those. Tracked.

**Hot-reload TLS toggle** (changing a listener from plain → TLS or
vice-versa) **does not** work via SIGHUP and never will — the socket
type changes. Restart the process.

---

## 4. Observability

### 4.1 Prometheus metrics

Expose `/metrics` on an internal listener:

```nginx
server {
    listen 127.0.0.1:9100;     # internal only
    location /metrics { metrics; }
}
```

Scrape with Prometheus:

```yaml
scrape_configs:
  - job_name: elrond
    static_configs: [{ targets: ['elrond:9100'] }]
```

Key metrics (full set documented at `/metrics`):

| Metric | Notes |
|---|---|
| `elrond_requests_total{status_class="2xx\|3xx\|4xx\|5xx"}` | Request volume by status class. |
| `elrond_proxy_attempts_total` / `_failures_total` | Upstream-side health view. |
| `elrond_tls_handshakes_total` / `_failures_total` | TLS health. |
| `elrond_cache_hits_total` / `_misses_total` / `_bypass_total` | Cache effectiveness. |
| `elrond_limit_req_throttled_total` / `_limit_conn_rejected_total` | Rate-limit pressure. |
| `elrond_connections_active` (gauge) | Currently open TCP connections. |

### 4.2 Access / error log

Set the two paths in the config:

```nginx
error_log /var/log/elrond/error.log;
http {
    access_log /var/log/elrond/access.log;
}
```

Each line is one request:

```
2026-05-16T04:23:51.226270Z  INFO 127.0.0.1 "GET /" 200 (h3)
```

Suffixes in parens explain *why* a request reached its status:
`(h3)` HTTP/3 path, `(client_max_body_size)`, `(allow/deny)`,
`(limit_req)`, `(limit_conn)`, `(auth_request)`, `(auth_basic challenge)`.

Logrotate: ship `examples/logrotate.elrond` and Elrond reopens both
log files on `SIGUSR1`.

---

## 5. Sizing / tuning

| Knob | Default | Recommended for production |
|---|---|---|
| `worker_rlimit_nofile` | (process default, often 1024) | `65536` minimum. |
| `client_max_body_size` per server | 1 MiB | Set explicitly; `0` for "unlimited" (use sparingly). |
| `proxy_connect_timeout` per location | 10 s | 2–5 s for east-west, 10 s for internet upstreams. |
| `proxy_read_timeout` per location | 60 s | Match your slowest legitimate upstream. |
| `keepalive_timeout` (parsed but not enforced) | — | Not yet a knob in Elrond. |
| `limit_req` per location | none | Always set for public surfaces. |

Memory: Elrond is single-process, multi-threaded. RSS scales with
**active connection count + cache size + buffered request bodies**.
A reasonable starting point is `expected concurrent connections × 80 KiB`
plus the configured `proxy_cache_path keys_zone=…` size in bytes.

---

## 6. Troubleshooting

### Cannot bind `:443`: permission denied

Either start as root (preferred — privileges are dropped immediately),
or grant the binary `CAP_NET_BIND_SERVICE`:

```bash
sudo setcap 'cap_net_bind_service=+ep' /usr/local/sbin/elrond
```

…or use systemd:

```ini
[Service]
AmbientCapabilities=CAP_NET_BIND_SERVICE
```

### `could not drop privileges to uid=… gid=…: Operation not permitted`

The process didn't start as root. Either remove the `user` directive
(if running as the target user already), or fix the launcher to start
elrond as root. Elrond refuses to keep running with the wrong identity.

### `worker_rlimit_nofile: already at soft limit X`

X is your current soft limit. If X is lower than your `worker_rlimit_nofile`
target, the hard cap is also too low; raise both at the OS level
(systemd unit `LimitNOFILE=`, or `/etc/security/limits.conf`).

### A reload silently kept the old config

Look at `error.log` — the reload validator surfaces the parse error
there. Elrond never displaces a running config with a broken one
(by design).

### HTTP/3 works for a while, then stops after I rotated certs

Known limitation: TCP TLS hot-reloads; the QUIC endpoint does not.
Restart the process to pick up the new cert on h3.

### A cert change took effect but old browser sessions still see the old cert

That's TLS session resumption, not Elrond. Force a fresh handshake
on the client. Resumption tickets are valid for their full lifetime
even after the cert that issued them changes.

---

## 7. Capacity smoke test (before going live)

Run from a separate host (so you measure the network too):

```bash
# Sustained throughput, mixed h1/h2/h3
hey -z 30s -c 200 -H "Host: yoursite.test" https://elrond-host/
h2load -t 4 -c 200 -n 50000 https://elrond-host/
curl --http3 -o /dev/null -s -w "%{time_total}\n" https://elrond-host/   # ×100

# Watch metrics in parallel
watch -n 1 'curl -s http://elrond-host:9100/metrics | grep -E "requests_total|proxy_(attempts|failures)|tls_handshakes" | head -20'
```

Acceptance gates (suggestion — tune to your stack):

- p99 of `time_total` for a 200-byte response < 50 ms over the LAN.
- `elrond_tls_handshakes_failures_total` stays at zero.
- `elrond_proxy_failures_total / elrond_proxy_attempts_total` < 0.1 %
  (the budget is "spurious upstream blips"; anything higher means
  the upstream is the problem).
- `elrond_connections_active` rises and falls cleanly with load; no
  long flat plateaus after load stops (= connection leak).

---

## 8. What still needs operator attention (won't change in 0.x)

Track these on your runbook until the upstream features ship:

- **Cert renewal**: Elrond doesn't ship ACME yet. Use external
  `certbot` / `lego` / cert-manager → drop new files on disk → `SIGHUP`.
- **OCSP stapling**: not yet supported.
- **OpenTelemetry traces**: not yet exported. Use Prometheus + log
  correlation for now.
- **Disk cache persistence**: the in-memory cache is wiped on
  restart. Plan for cache cold-start.
- **`stale-while-revalidate`**: not yet honored.
