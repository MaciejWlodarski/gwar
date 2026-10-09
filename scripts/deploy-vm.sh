#!/usr/bin/env bash
# Builds and (re)starts a server on a Linux host over SSH (systemd user unit).
#
#   HTTP/WS   : 127.0.0.1:8800 on the VM, published by nginx (or: ssh -L 8800:127.0.0.1:8800 oracle)
#   voice     : UDP 9988 (WebRTC) on the VM's private address; the cloud NAT
#               maps the public IP to it, so browsers get PUBLIC_IP:9988
#   TeamSpeak : UDP 9987, the bridged official TeamSpeak server (VC_TEAMSPEAK=0 disables it;
#               enabling it accepts the TeamSpeak server license)
#   Connect   : 127.0.0.1:8900, Gwar Connect (VC_CONNECT=0 skips it); nginx publishes it
#               under /connect/ (see docs/deploy.md)
#   Backups   : daily copies of both databases in ~/gwar-backups, kept 14 days
#
# Usage: scripts/deploy-vm.sh [--web]   (--web also builds apps/web locally and uploads dist)
set -euo pipefail
cd "$(dirname "$0")/.."
# Host-specific settings (VC_DEPLOY_HOST, VC_PUBLIC_IP, VC_PRIVATE_IP, ...) live
# in scripts/deploy.env, which is not committed.
if [[ -f scripts/deploy.env ]]; then
  # shellcheck source=/dev/null
  source scripts/deploy.env
fi
HOST=${VC_DEPLOY_HOST:?set VC_DEPLOY_HOST (ssh host) in scripts/deploy.env}
PRIVATE_IP=${VC_PRIVATE_IP:-0.0.0.0}
# Fail fast instead of hanging when the connection drops mid-build.
SSH_OPTS="-o ConnectTimeout=20 -o ServerAliveInterval=15 -o ServerAliveCountMax=8"
TEAMSPEAK=${VC_TEAMSPEAK:-1}
if [[ "$TEAMSPEAK" == 1 ]]; then
  MEDIA_BIND=${VC_MEDIA_BIND:-$PRIVATE_IP:9988}
  TS_ARGS="--teamspeak --accept-teamspeak-license --teamspeak-voice ${VC_TEAMSPEAK_VOICE:-$PRIVATE_IP:9987}"
else
  MEDIA_BIND=${VC_MEDIA_BIND:-$PRIVATE_IP:9987}
  TS_ARGS=""
fi
PUBLIC_IP=${VC_PUBLIC_IP:?set VC_PUBLIC_IP (address clients reach) in scripts/deploy.env}
# Extra vc-server flags
EXTRA_ARGS="$TS_ARGS ${VC_EXTRA_ARGS:-}"
CONNECT=${VC_CONNECT:-1}
CONNECT_ARGS=${VC_CONNECT_ARGS:-}

if [[ "${1:-}" == "--web" ]]; then
  pnpm --filter web build
fi

rsync -az -e "ssh $SSH_OPTS" --delete --exclude target --exclude node_modules --exclude .git --exclude .claude \
  --exclude 'apps/web/e2e/screenshots' ./ "$HOST:vc/src/"

# The remote shell re-parses the command line, so quote the values for it.
ssh $SSH_OPTS -T "$HOST" MEDIA_BIND="$MEDIA_BIND" PUBLIC_IP="$PUBLIC_IP" EXTRA_ARGS="$(printf %q "$EXTRA_ARGS")" \
  CONNECT="$CONNECT" CONNECT_ARGS="$(printf %q "$CONNECT_ARGS")" \
  timeout 1500 bash -s <<'REMOTE'
set -euo pipefail
export PATH=$HOME/.cargo/bin:$PATH
cd ~/vc/src
PACKAGES="-p vc-server"
[[ "$CONNECT" == 1 ]] && PACKAGES="$PACKAGES -p gwar-connect"
CARGO_TARGET_DIR=$HOME/vc/target nice cargo build --release $PACKAGES
install -m 755 ~/vc/target/release/vc-server ~/vc/vc-server
mkdir -p ~/.config/systemd/user ~/vc/data ~/gwar-backups
cat > ~/.config/systemd/user/vc-server.service <<UNIT
[Unit]
Description=Voice communicator server (development)

[Service]
WorkingDirectory=%h/vc
ExecStart=%h/vc/vc-server --data-dir %h/vc/data --http 127.0.0.1:8800 --media ${MEDIA_BIND} --public-ip ${PUBLIC_IP} --web-root %h/vc/src/apps/web/dist ${EXTRA_ARGS}
Restart=on-failure
UMask=0077

[Install]
WantedBy=default.target
UNIT

if [[ "$CONNECT" == 1 ]]; then
  mkdir -p ~/gwar-connect/data
  install -m 755 ~/vc/target/release/gwar-connect ~/gwar-connect/gwar-connect
  cat > ~/.config/systemd/user/gwar-connect.service <<UNIT
[Unit]
Description=Gwar Connect (accounts)

[Service]
WorkingDirectory=%h/gwar-connect
ExecStart=%h/gwar-connect/gwar-connect --data-dir %h/gwar-connect/data --http 127.0.0.1:8900 ${CONNECT_ARGS}
Restart=on-failure
UMask=0077

[Install]
WantedBy=default.target
UNIT
fi

# Daily consistent copies of the databases (VACUUM INTO), kept for 14 days.
cat > ~/gwar-backups/backup.sh <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
day=$(date +%F)
cd ~/gwar-backups
~/vc/vc-server --data-dir ~/vc/data backup "server-$day.sqlite3.tmp" >/dev/null
mv -f "server-$day.sqlite3.tmp" "server-$day.sqlite3"
if [[ -x ~/gwar-connect/gwar-connect ]]; then
  ~/gwar-connect/gwar-connect --data-dir ~/gwar-connect/data backup "connect-$day.sqlite3.tmp" >/dev/null
  mv -f "connect-$day.sqlite3.tmp" "connect-$day.sqlite3"
fi
find ~/gwar-backups -name '*.sqlite3' -mtime +14 -delete
SCRIPT
chmod 700 ~/gwar-backups/backup.sh
cat > ~/.config/systemd/user/gwar-backup.service <<'UNIT'
[Unit]
Description=Back up the Gwar databases

[Service]
Type=oneshot
ExecStart=%h/gwar-backups/backup.sh
UMask=0077
UNIT
cat > ~/.config/systemd/user/gwar-backup.timer <<'UNIT'
[Unit]
Description=Daily backup of the Gwar databases

[Timer]
OnCalendar=*-*-* 04:20:00
Persistent=true

[Install]
WantedBy=timers.target
UNIT

systemctl --user daemon-reload
systemctl --user enable --now gwar-backup.timer
systemctl --user enable --now vc-server.service
systemctl --user restart vc-server.service
if [[ "$CONNECT" == 1 ]]; then
  systemctl --user enable --now gwar-connect.service
  systemctl --user restart gwar-connect.service
fi
sleep 1
systemctl --user is-active vc-server.service
curl -fsS -m 5 http://127.0.0.1:8800/health && echo
if [[ "$CONNECT" == 1 ]]; then
  systemctl --user is-active gwar-connect.service
  curl -fsS -m 5 http://127.0.0.1:8900/health && echo
fi
REMOTE
