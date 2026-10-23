#!/usr/bin/env bash
# Ubuntu runners only. Bound both mirrors, including Playwright's apt subprocesses.
set -euo pipefail
if (( $# == 0 )); then
  echo 'usage: apt-install.sh PACKAGE... | --playwright BROWSER...' >&2
  exit 2
fi
sudo tee /etc/apt/apt.conf.d/80gwar-ci >/dev/null <<'EOF'
Acquire::Retries "1";
Acquire::http::Timeout "15";
Acquire::https::Timeout "15";
APT::Install-Recommends "false";
APT::Update::Error-Mode "any";
EOF

for mirror in https://archive.ubuntu.com/ubuntu https://mirrors.edge.kernel.org/ubuntu; do
  echo "Using Ubuntu mirror: $mirror"
  sudo python3 scripts/ci/apt-sources.py "$mirror"
  # One attempt takes at most 75 s update + 135 s install (+ 10 s kill grace each).
  if timeout --kill-after=10s 75s sudo apt-get update; then
    if [[ $1 == --playwright ]]; then
      if timeout --kill-after=10s 135s pnpm --filter web exec playwright install-deps "${@:2}"; then
        exit 0
      fi
    elif timeout --kill-after=10s 135s sudo apt-get install -y --no-install-recommends "$@"; then
      exit 0
    fi
  fi
  echo "Ubuntu mirror attempt failed or timed out: $mirror" >&2
done
echo 'Both Ubuntu mirrors failed within the bounded install attempts' >&2
exit 1
