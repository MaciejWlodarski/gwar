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

nginx terminates HTTPS (certbot) for both names.

`gwar.maciejwlodarski.com` sits behind Cloudflare, so the real client address
comes from `CF-Connecting-IP`, trusted only from Cloudflare's ranges
(`/etc/nginx/snippets/cloudflare-realip.conf`, generated from
<https://www.cloudflare.com/ips/>: `set_real_ip_from …;` per range plus
`real_ip_header CF-Connecting-IP;`). Connect's rate limits depend on it.

```nginx
server {
    server_name gwar.maciejwlodarski.com;
    include snippets/cloudflare-realip.conf;
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
        add_header Cache-Control "public, max-age=31536000, immutable";
        try_files $uri =404;
    }
    location / {
        add_header Cache-Control "no-cache";
        try_files $uri /index.html;
    }
    # listen 443 ssl; ... (managed by certbot)
}

server {
    server_name voice.maciejwlodarski.com;
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

Both services trust forwarded addresses only from loopback, so they must not be
published directly.

## Restoring a backup

Stop the service, replace its database with a backup copy and start it again:

```sh
systemctl --user stop gwar-connect
cp ~/gwar-backups/connect-2026-10-09.sqlite3 ~/gwar-connect/data/connect.sqlite3
rm -f ~/gwar-connect/data/connect.sqlite3-wal ~/gwar-connect/data/connect.sqlite3-shm
systemctl --user start gwar-connect
```

The same works for `vc-server` with `~/vc/data/vc.sqlite3`.
