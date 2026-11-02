#!/usr/bin/env bash
# One CI runner: every arm of the row, interleaved, ROUNDS times; viewer order alternates.
# Row settings come from .github/ab213.env (NET, SECONDS_, ROUNDS, ARMS, ROW_ENV).
set -u
source .github/ab213.env
out="$RUNNER_TEMP/ab"; mkdir -p "$out"
{ nproc; lscpu | grep -E "Model name|^CPU\(s\)"; cat /proc/meminfo | head -1; } > "$out/runner-$SHARD.txt"
pulseaudio --start --exit-idle-time=-1
pactl load-module module-null-sink sink_name=e2e >/dev/null
pactl set-default-sink e2e
read -ra arms <<< "$ARMS"
n=${#arms[@]}
for round in $(seq 1 "$ROUNDS"); do
  viewers=desktop,browser; (( (round + SHARD) % 2 == 0 )) && viewers=browser,desktop
  for k in $(seq 0 $((n - 1))); do
    arm=${arms[$(( (k + round + SHARD) % n ))]}
    name="$arm-s$SHARD-r$round"
    unset SYNC_OFF SYNC_NOHOLD SYNC_NOFILTER
    case $arm in off) export SYNC_OFF=1;; nohold) export SYNC_NOHOLD=1;; nofilter) export SYNC_NOFILTER=1;; neither) export SYNC_NOHOLD=1 SYNC_NOFILTER=1;; esac
    vmstat 1 > "$out/$name.vmstat" & vm=$!
    env $ROW_ENV SYNC_NET="$NET" SYNC_SECONDS="$SECONDS_" SYNC_VIEWERS=$viewers SYNC_OUT="$out/$name.json" \
      timeout 600 node apps/web/e2e/share-sync.mjs > "$out/$name.log" 2>&1
    echo "$name exit $?"
    kill $vm
  done
done
exit 0
