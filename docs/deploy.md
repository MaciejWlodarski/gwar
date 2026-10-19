# Deploying the official services

How the official services run. A self-hosted server needs none of this:
`gwar-server --domain …` is enough (see the README).

| Address | What | DNS |
| --- | --- | --- |
| `gwar.app` | the web app (static), Gwar Connect under `/connect/` and the project's public Gwar server (`/health`, `/ws`, `/api/files/`, `/files/`, `/api/mls/`) | DNS only |
| `www.gwar.app` | redirects to `gwar.app` (nginx, below) | CNAME to `gwar.app` |
| `voice.gwar.app` | the same Gwar server under its own name; TeamSpeak voice (UDP) and file transfer (TCP 30033) | DNS only: voice is UDP straight to the host |
| `_ts3._udp.gwar.app` | SRV record to `voice.gwar.app` port 9987, so TeamSpeak clients can type `gwar.app` | — |

Keep `gwar.app` DNS only: a Cloudflare proxy would terminate TLS for the
server's WebSocket and see every chat message and upload in the clear.

`scripts/deploy-vm.sh [--web]` copies the source to the host, builds
`gwar-server` and `gwar-connect` there and installs them as systemd user units:

| Unit | Listens on | Data |
| --- | --- | --- |
| `gwar-server.service` | HTTP `127.0.0.1:8800`, WebRTC UDP 9988, TeamSpeak UDP 9987 and TCP 30033 | `~/gwar/data` |
| `gwar-connect.service` | HTTP `127.0.0.1:8900` | `~/gwar-connect/data` |
| `gwar-backup.timer` | daily at 04:20 | copies in `~/gwar-backups`, kept 14 days |

Host settings live in `scripts/deploy.env` (not committed): `GWAR_DEPLOY_HOST`,
`GWAR_PUBLIC_IP`, `GWAR_PRIVATE_IP`, `GWAR_EXTRA_ARGS`, `GWAR_CONNECT` (0 skips
Connect), `GWAR_CONNECT_ARGS`, `GWAR_WEB_DIR` (where `--web` publishes the web
app, `/var/www/gwar`, owned by the deploy user).

For user units to keep running without a login session, enable lingering once:
`loginctl enable-linger $USER`.

## nginx

nginx terminates HTTPS (certbot) for these names. The server runs without
`--tls`; `--public-url https://gwar.app` authorizes `wss://gwar.app/ws` and
`--public-endpoint wss://voice.gwar.app/ws` the second name. `--public-ip` advertises UDP media and
adds no login endpoint behind nginx. Additional externally reachable aliases
need a repeatable `--public-endpoint` with their exact scheme, port and path.
Never use `--insecure-any-host` for these services.

`scripts/deploy-vm.sh --web` publishes the web app only from a tree without
uncommitted changes, untracked sources or local `.env` files, builds it without
any `VITE_*` overrides, checks that nothing changed during the build, and writes
`version.json` (`{"commit": "<sha>"}`) next to it, so the live app names the
commit it was built from.

The production web build includes a CSP meta policy: local scripts, WASM for
hash-wasm's Argon2, HTTPS/WSS connections, and bounded attachment blobs. Vite dev
omits it for HMR and loopback smoke tests. The desktop uses `build:desktop` and
its native CSP to permit IPC and explicitly selected LAN transports.

Hosting must also set these HTTP headers. `frame-ancestors` is ignored in a
meta policy; `nosniff`, Referrer-Policy and HSTS must be response headers. Put the
following in `/etc/nginx/snippets/gwar-security-headers.conf` and include it in
each static location that declares `add_header` (nginx header inheritance does
not cross such locations):

```nginx
add_header Content-Security-Policy "frame-ancestors 'none'" always;
add_header X-Content-Type-Options "nosniff" always;
add_header Referrer-Policy "no-referrer" always;
add_header Strict-Transport-Security "max-age=31536000" always;
```

Serve HTTPS only, redirect port 80 to HTTPS, and add `includeSubDomains` to HSTS
only when every subdomain supports HTTPS. The header CSP combines with the
meta policy; it does not replace it.


If a name served this way is ever proxied by Cloudflare, the real client address
comes from `CF-Connecting-IP`, trusted only from Cloudflare's ranges
(`/etc/nginx/snippets/cloudflare-realip.conf`, generated from
<https://www.cloudflare.com/ips/>: `set_real_ip_from …;` per range plus
`real_ip_header CF-Connecting-IP;`). Connect's rate limits depend on it.

```nginx
map $http_upgrade $gwar_connection_upgrade {
    default upgrade;
    ""      close;
}

server {
    server_name gwar.app;
    include snippets/cloudflare-realip.conf;
    include snippets/gwar-security-headers.conf;
    root /var/www/gwar;
    index index.html;

    location /connect/ {
        proxy_pass http://127.0.0.1:8900/;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_set_header X-Forwarded-Proto https;
    }
    # Bug reports carry screenshots (Connect accepts up to about 28 MiB here).
    location = /connect/v1/reports {
        client_max_body_size 30m;
        proxy_pass http://127.0.0.1:8900/v1/reports;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_set_header X-Forwarded-Proto https;
    }
    # The Gwar server's routes; the snippet is the proxy block of voice.gwar.app below.
    # Downloads share the web app's origin safely: only sniffed media is inline, and every
    # file is served with nosniff and `Content-Security-Policy: default-src 'none'; sandbox`.
    # The web app asks /health whether a Gwar server answers on its own origin.
    location = /health { include snippets/gwar-server-proxy.conf; }
    location = /ws { include snippets/gwar-server-proxy.conf; }
    location /api/files/ {
        client_max_body_size 30m;
        include snippets/gwar-server-proxy.conf;
    }
    location /files/ { include snippets/gwar-server-proxy.conf; }
    location /api/mls/ { include snippets/gwar-server-proxy.conf; }
    location /assets/ {
        include snippets/gwar-security-headers.conf;
        add_header Cache-Control "public, max-age=31536000, immutable";
        try_files $uri =404;
    }
    location / {
        include snippets/gwar-security-headers.conf;
        add_header Cache-Control "no-cache";
        try_files $uri /index.html;
    }
    # listen 443 ssl; ... (managed by certbot)
}

server {
    server_name voice.gwar.app;
    include snippets/gwar-security-headers.conf;
    client_max_body_size 30m;

    location / {
        # snippets/gwar-server-proxy.conf:
        proxy_pass http://127.0.0.1:8800;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $gwar_connection_upgrade;
        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
    }
    # listen 443 ssl; ... (managed by certbot)
}

server {
    server_name www.gwar.app;
    include snippets/gwar-security-headers.conf;
    return 301 https://gwar.app$request_uri;
    # listen 443 ssl; ... (managed by certbot)
}
```

nginx overwrites `X-Real-IP` with the address it sees, and the units run with
`--client-ip-header x-real-ip`, so per-address limits and bans see real clients.
The header is believed only from a loopback peer, so the services must not be
published directly. Without the option every proxied client shares nginx's
address (the server logs a warning); naming a header the proxy does not set
would let clients choose their own address. IPv6 clients are limited and banned
per /64.

## Restoring a backup

Stop the service, replace its database with a backup copy and start it again:

```sh
systemctl --user stop gwar-connect
cp ~/gwar-backups/connect-2026-10-09.sqlite3 ~/gwar-connect/data/connect.sqlite3
rm -f ~/gwar-connect/data/connect.sqlite3-wal ~/gwar-connect/data/connect.sqlite3-shm
systemctl --user start gwar-connect
```

The same works for `gwar-server` with `~/gwar/data/gwar.sqlite3`.

Back up `~/gwar/data/server-key.ed25519` together with the server database and
restore both. This private 32-byte seed identifies the server; keep its Unix
permissions at 0600. Losing it changes the identity and blocks pinned clients.
`gwar-server --data-dir ~/gwar/data server-key` prints the public fingerprint for
users to check. Clients pin each endpoint only after verifying the proof; invite
links carry `server_key`. A changed pin requires checking with the owner before
explicitly trusting the replacement. The deployment backup timer copies the
server key beside the Connect service key.
