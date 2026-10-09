#!/usr/bin/env bash
# Builds and (re)starts a server on a Linux host over SSH (systemd user unit).
#
#   HTTP/WS   : 127.0.0.1:8800 on the VM, published by nginx (or: ssh -L 8800:127.0.0.1:8800 oracle)
#   voice     : UDP 9988 (WebRTC) on the VM's private address; the cloud NAT
#               maps the public IP to it, so browsers get PUBLIC_IP:9988
#   TeamSpeak : UDP 9987, the bridged official TeamSpeak server (VC_TEAMSPEAK=0 disables it;
#               enabling it accepts the TeamSpeak server license)
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

if [[ "${1:-}" == "--web" ]]; then
  pnpm --filter web build
fi

rsync -az -e "ssh $SSH_OPTS" --delete --exclude target --exclude node_modules --exclude .git --exclude .claude \
  --exclude 'apps/web/e2e/screenshots' ./ "$HOST:vc/src/"

# The remote shell re-parses the command line, so quote the values for it.
ssh $SSH_OPTS -T "$HOST" MEDIA_BIND="$MEDIA_BIND" PUBLIC_IP="$PUBLIC_IP" EXTRA_ARGS="$(printf %q "$EXTRA_ARGS")" \
  timeout 1500 bash -s <<'REMOTE'
set -euo pipefail
export PATH=$HOME/.cargo/bin:$PATH
cd ~/vc/src
CARGO_TARGET_DIR=$HOME/vc/target nice cargo build --release -p vc-server
install -m 755 ~/vc/target/release/vc-server ~/vc/vc-server
mkdir -p ~/.config/systemd/user ~/vc/data
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
systemctl --user daemon-reload
systemctl --user enable --now vc-server.service
systemctl --user restart vc-server.service
sleep 1
systemctl --user is-active vc-server.service
curl -fsS -m 5 http://127.0.0.1:8800/health && echo
REMOTE
