# Deploying the official services

How the official services run. A self-hosted server needs none of this:
`vc-server --domain …` is enough (see the README).

| Address | What | DNS |
| --- | --- | --- |
| `gwar.maciejwlodarski.com` | the web app (static) and Gwar Connect under `/connect/` | proxied by Cloudflare |
| `voice.maciejwlodarski.com` | the project's Gwar server (WebSocket, files, UDP voice, TeamSpeak) | DNS only: voice is UDP straight to the host |

`scripts/deploy-vm.sh [--web]` copies the source to the host, builds
`vc-server` and `gwar-connect` there and installs them as systemd user units:

| Unit | Listens on | Data |
| --- | --- | --- |
| `vc-server.service` | HTTP `127.0.0.1:8800`, WebRTC UDP 9988, TeamSpeak UDP 9987 | `~/vc/data` |
| `gwar-connect.service` | HTTP `127.0.0.1:8900` | `~/gwar-connect/data` |
| `gwar-backup.timer` | daily at 04:20 | copies in `~/gwar-backups`, kept 14 days |

Host settings live in `scripts/deploy.env` (not committed): `VC_DEPLOY_HOST`,
`VC_PUBLIC_IP`, `VC_PRIVATE_IP`, `VC_EXTRA_ARGS`, `VC_CONNECT` (0 skips
Connect), `VC_CONNECT_ARGS`, `VC_WEB_DIR` (where `--web` publishes the web
app, `/var/www/gwar`, owned by the deploy user).

For user units to keep running without a login session, enable lingering once:
`loginctl enable-linger $USER`.

## nginx

nginx terminates HTTPS (certbot) for both names. The server runs without
`--tls`; set `--public-url https://voice.maciejwlodarski.com` to authorize
`wss://voice.maciejwlodarski.com/ws`. `--public-ip` advertises UDP media and
adds no login endpoint behind nginx. Additional externally reachable aliases
need a repeatable `--public-endpoint` with their exact scheme, port and path.
Never use `--insecure-any-host` for these services.

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


`gwar.maciejwlodarski.com` sits behind Cloudflare, so the real client address
comes from `CF-Connecting-IP`, trusted only from Cloudflare's ranges
(`/etc/nginx/snippets/cloudflare-realip.conf`, generated from
<https://www.cloudflare.com/ips/>: `set_real_ip_from …;` per range plus
`real_ip_header CF-Connecting-IP;`). Connect's rate limits depend on it.

```nginx
server {
    server_name gwar.maciejwlodarski.com;
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
    server_name voice.maciejwlodarski.com;
    include snippets/gwar-security-headers.conf;
    client_max_body_size 30m;

    location / {
        proxy_pass http://127.0.0.1:8800;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;
        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
    }
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

The same works for `vc-server` with `~/vc/data/vc.sqlite3`.

Back up `~/vc/data/server-key.ed25519` together with the server database and
restore both. This private 32-byte seed identifies the server; keep its Unix
permissions at 0600. Losing it changes the identity and blocks pinned clients.
`vc-server --data-dir ~/vc/data server-key` prints the public fingerprint for
users to check. Clients pin each endpoint only after verifying the proof; invite
links carry `server_key`. A changed pin requires checking with the owner before
explicitly trusting the replacement. The deployment backup timer copies the
server key beside the Connect service key.
