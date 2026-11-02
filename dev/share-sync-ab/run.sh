#!/bin/zsh
# usage: TREE=<worktree> TARGET=<cargo target> run.sh <net> <out.json> [viewers] [seconds]
# Writes <out>.meta: load at start and end, start and end time. The run is void if the 1-min load
# went above 6 at either end, or the Mac slept in between (checked by valid.sh).
cd ${TREE:?}/apps/web
export CARGO_TARGET_DIR=${TARGET:?} SYNC_NET=$1 SYNC_OUT=$2 SYNC_VIEWERS=${3:-desktop,browser} SYNC_SECONDS=${4:-30} VITE_CONNECT_URL=http://127.0.0.1:18980
meta=${2%.json}.meta
echo "start $(date +%s) $(sysctl -n vm.loadavg | awk '{print $2}')" > $meta
here=${0:A:h}; work=${SS_WORK:-/tmp/ss213}; log=$work/logs/run-$$.log; mkdir -p $work/logs
node e2e/share-sync.mjs > $log 2>&1 &
pid=$!
python3 $here/guard.py $pid > $work/logs/guard-$$.log 2>&1 &
wait $pid
code=$?
echo "end $(date +%s) $(sysctl -n vm.loadavg | awk '{print $2}')" >> $meta
[ -s $work/logs/guard-$$.log ] && { cat $work/logs/guard-$$.log | cut -c1-120; }
[ $code -ne 0 ] && tail -5 $log | cut -c1-200
echo "$(basename $2) exit $code"
