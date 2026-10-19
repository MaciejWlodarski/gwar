#!/usr/bin/env bash
# Builds and (re)starts a server on a Linux host over SSH (systemd user unit).
#
#   HTTP/WS   : 127.0.0.1:8800 on the VM, published by nginx (or: ssh -L 8800:127.0.0.1:8800 oracle)
#   voice     : UDP 9988 (WebRTC) on the VM's private address; the cloud NAT
#               maps the public IP to it, so browsers get PUBLIC_IP:9988
#   TeamSpeak : UDP 9987, the bridged official TeamSpeak server (GWAR_TEAMSPEAK=0 disables it;
#               enabling it accepts the TeamSpeak server license), and its file
#               transfer on TCP 30033 (all addresses, so clients use the public IP)
#   Connect   : 127.0.0.1:8900, Gwar Connect (GWAR_CONNECT=0 skips it); nginx publishes it
#               under /connect/ (see docs/deploy.md)
#   Backups   : daily copies of both databases in ~/gwar-backups, kept 14 days
#   Web app   : with --web, dist is also published to GWAR_WEB_DIR (a static nginx root)
#
# Usage: scripts/deploy-vm.sh [--web]   (--web also builds apps/web locally and uploads dist)
set -euo pipefail
cd "$(dirname "$0")/.."
# Host-specific settings (GWAR_DEPLOY_HOST, GWAR_PUBLIC_IP, GWAR_PRIVATE_IP, ...) live
# in scripts/deploy.env, which is not committed.
if [[ -f scripts/deploy.env ]]; then
  # shellcheck source=/dev/null
  source scripts/deploy.env
fi
HOST=${GWAR_DEPLOY_HOST:?set GWAR_DEPLOY_HOST (ssh host) in scripts/deploy.env}
PRIVATE_IP=${GWAR_PRIVATE_IP:-0.0.0.0}
# Fail fast instead of hanging when the connection drops mid-build.
SSH_OPTS="-o ConnectTimeout=20 -o ServerAliveInterval=15 -o ServerAliveCountMax=8"
TEAMSPEAK=${GWAR_TEAMSPEAK:-1}
if [[ "$TEAMSPEAK" == 1 ]]; then
  MEDIA_BIND=${GWAR_MEDIA_BIND:-$PRIVATE_IP:9988}
  TS_ARGS="--teamspeak --accept-teamspeak-license --teamspeak-voice ${GWAR_TEAMSPEAK_VOICE:-$PRIVATE_IP:9987}"
else
  MEDIA_BIND=${GWAR_MEDIA_BIND:-$PRIVATE_IP:9987}
  TS_ARGS=""
fi
PUBLIC_IP=${GWAR_PUBLIC_IP:?set GWAR_PUBLIC_IP (address clients reach) in scripts/deploy.env}
# Extra gwar-server flags
EXTRA_ARGS="$TS_ARGS ${GWAR_EXTRA_ARGS:-}"
CONNECT=${GWAR_CONNECT:-1}
CONNECT_ARGS=${GWAR_CONNECT_ARGS:-}
WEB_DIR=${GWAR_WEB_DIR:-}
PUBLISH_WEB=0

# The published app names the commit it was built from (README, "The official web app"), so it must be
# exactly that commit: no uncommitted or untracked sources, no local .env files, no VITE_* overrides.
web_tree_clean() {
  git diff --quiet HEAD -- &&
    [[ -z "$(git ls-files --others --exclude-standard -- apps crates)" ]] &&
    [[ -z "$(git ls-files --others --ignored --exclude-standard -- apps/web | grep -E '(^|/)\.env' || true)" ]]
}
if [[ "${1:-}" == "--web" ]]; then
  if ! web_tree_clean; then
    echo "refusing to publish the web app: uncommitted changes, untracked sources or a local .env file" >&2
    exit 1
  fi
  commit=$(git rev-parse HEAD)
  # The app embeds the MLS wasm of this commit. Use the tools CI pins: wasm-bindgen-cli 0.2.129
  # and binaryen 123.0.0 (WASM_BINDGEN / WASM_OPT, or on PATH).
  if ! scripts/mls-wasm/build.sh; then
    echo "refusing to publish the web app: the MLS wasm did not build (see scripts/mls-wasm/build.sh)" >&2
    exit 1
  fi
  # shellcheck disable=SC2046
  env $(env | grep -o '^VITE_[A-Za-z0-9_]*' | sed 's/^/-u /') pnpm --filter web build
  if [[ "$(git rev-parse HEAD)" != "$commit" ]] || ! web_tree_clean; then
    echo "refusing to publish the web app: the tree changed during the build" >&2
    exit 1
  fi
  printf '{"commit":"%s"}\n' "$commit" > apps/web/dist/version.json
  PUBLISH_WEB=1
fi

# Before the rename to Gwar everything lived in ~/vc. Deploying beside it would start an empty server.
# Exit 3 means the old layout; anything else but 0 (ssh failing) stops too.
layout=0
ssh $SSH_OPTS -T "$HOST" 'if test -d ~/vc && ! test -d ~/gwar; then exit 3; fi' || layout=$?
if [[ "$layout" != 0 && "$layout" != 3 ]]; then
  echo "refusing to deploy: could not check $HOST (ssh exit $layout)" >&2
  exit 1
fi
if [[ "$layout" == 3 ]]; then
  echo "refusing to deploy: $HOST still has ~/vc from before the rename. On the VM: stop and disable" >&2
  echo "vc-server.service, remove its unit, mv ~/vc ~/gwar, rename data/vc.sqlite3 (and -wal, -shm) to" >&2
  echo "gwar.sqlite3, delete data/vc.lock, then deploy again." >&2
  exit 1
fi

rsync -az -e "ssh $SSH_OPTS" --delete --exclude target --exclude node_modules --exclude .git --exclude .claude \
  --exclude 'apps/web/e2e/screenshots' ./ "$HOST:gwar/src/"

# The remote shell re-parses the command line, so quote the values for it.
ssh $SSH_OPTS -T "$HOST" MEDIA_BIND="$MEDIA_BIND" PUBLIC_IP="$PUBLIC_IP" EXTRA_ARGS="$(printf %q "$EXTRA_ARGS")" \
  CONNECT="$CONNECT" CONNECT_ARGS="$(printf %q "$CONNECT_ARGS")" WEB_DIR="$WEB_DIR" PUBLISH_WEB="$PUBLISH_WEB" \
  timeout 1500 bash -s <<'REMOTE'
set -euo pipefail
export PATH=$HOME/.cargo/bin:$PATH
cd ~/gwar/src
PACKAGES="-p gwar-server"
[[ "$CONNECT" == 1 ]] && PACKAGES="$PACKAGES -p gwar-connect"
CARGO_TARGET_DIR=$HOME/gwar/target nice cargo build --release $PACKAGES
install -m 755 ~/gwar/target/release/gwar-server ~/gwar/gwar-server
mkdir -p ~/.config/systemd/user ~/gwar/data ~/gwar-backups
cat > ~/.config/systemd/user/gwar-server.service <<UNIT
[Unit]
Description=Voice communicator server (development)

[Service]
WorkingDirectory=%h/gwar
ExecStart=%h/gwar/gwar-server --data-dir %h/gwar/data --http 127.0.0.1:8800 --client-ip-header x-real-ip --media ${MEDIA_BIND} --public-ip ${PUBLIC_IP} --web-root %h/gwar/src/apps/web/dist ${EXTRA_ARGS}
Restart=on-failure
UMask=0077

[Install]
WantedBy=default.target
UNIT

if [[ "$CONNECT" == 1 ]]; then
  mkdir -p ~/gwar-connect/data
  install -m 755 ~/gwar/target/release/gwar-connect ~/gwar-connect/gwar-connect
  cat > ~/.config/systemd/user/gwar-connect.service <<UNIT
[Unit]
Description=Gwar Connect (accounts)

[Service]
WorkingDirectory=%h/gwar-connect
ExecStart=%h/gwar-connect/gwar-connect --data-dir %h/gwar-connect/data --http 127.0.0.1:8900 --client-ip-header x-real-ip ${CONNECT_ARGS}
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
~/gwar/gwar-server --data-dir ~/gwar/data backup "server-$day.sqlite3.tmp" >/dev/null
mv -f "server-$day.sqlite3.tmp" "server-$day.sqlite3"
if [[ -f ~/gwar/data/server-key.ed25519 ]]; then
  install -m 600 ~/gwar/data/server-key.ed25519 server-key.ed25519
fi
if [[ -x ~/gwar-connect/gwar-connect ]]; then
  ~/gwar-connect/gwar-connect --data-dir ~/gwar-connect/data backup "connect-$day.sqlite3.tmp" >/dev/null
  mv -f "connect-$day.sqlite3.tmp" "connect-$day.sqlite3"
  # Signs handle attestations and never changes: keep one private copy.
  if [[ -f ~/gwar-connect/data/service-key.ed25519 ]]; then
    install -m 600 ~/gwar-connect/data/service-key.ed25519 connect-service-key.ed25519
  fi
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

# Only now: a web app published before a failed server build would face the old server.
if [[ "$PUBLISH_WEB" == 1 && -n "$WEB_DIR" ]]; then
  rsync -a --delete apps/web/dist/ "$WEB_DIR/"
  echo "web app published to $WEB_DIR"
fi
systemctl --user daemon-reload
systemctl --user enable --now gwar-backup.timer
systemctl --user enable --now gwar-server.service
systemctl --user restart gwar-server.service
if [[ "$CONNECT" == 1 ]]; then
  systemctl --user enable --now gwar-connect.service
  systemctl --user restart gwar-connect.service
fi
sleep 1
systemctl --user is-active gwar-server.service
curl -fsS -m 5 http://127.0.0.1:8800/health && echo
if [[ "$CONNECT" == 1 ]]; then
  systemctl --user is-active gwar-connect.service
  curl -fsS -m 5 http://127.0.0.1:8900/health && echo
fi
REMOTE
