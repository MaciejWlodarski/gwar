# Deploying the official services

How the official instance (currently `voice.maciejwlodarski.com`) runs. A
self-hosted server needs none of this: `vc-server --domain …` is enough (see
the README).

`scripts/deploy-vm.sh [--web]` copies the source to the host, builds
`vc-server` and `gwar-connect` there and installs them as systemd user units:

| Unit | Listens on | Data |
| --- | --- | --- |
| `vc-server.service` | HTTP `127.0.0.1:8800`, WebRTC UDP 9988, TeamSpeak UDP 9987 | `~/vc/data` |
| `gwar-connect.service` | HTTP `127.0.0.1:8900` | `~/gwar-connect/data` |
| `gwar-backup.timer` | daily at 04:20 | copies in `~/gwar-backups`, kept 14 days |

Host settings live in `scripts/deploy.env` (not committed): `VC_DEPLOY_HOST`,
`VC_PUBLIC_IP`, `VC_PRIVATE_IP`, `VC_EXTRA_ARGS`, `VC_CONNECT` (0 skips
Connect), `VC_CONNECT_ARGS`.

For user units to keep running without a login session, enable lingering once:
`loginctl enable-linger $USER`.

## nginx

nginx terminates HTTPS (certbot) and publishes both services on one domain.
Connect is mounted under `/connect/` with the prefix stripped:

```nginx
server {
    server_name voice.maciejwlodarski.com;
    client_max_body_size 30m;

    location /connect/ {
        proxy_pass http://127.0.0.1:8900/;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }

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
