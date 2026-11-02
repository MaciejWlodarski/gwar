#!/bin/zsh
# Interleaved arms of one row, both viewers, viewer order alternating; void runs re-run.
# usage: sync.sh <name> <net> <seconds> <from> <to> <arm>...   arms: on off nohold nofilter neither
# extra env for every arm: ROW_ENV="SYNC_RETURN=1" etc.
name=$1 net=$2 secs=$3 from=$4 to=$5; shift 5
here=${0:A:h}; out=${SS_WORK:-/tmp/ss213}/data/$name; mkdir -p $out
export TREE=${TREE:-$(git -C $here rev-parse --show-toplevel)} TARGET=${TARGET:-/tmp/ss213-target}
for pass in 1 2 3; do
 for i in $(seq $from $to); do
  for arm in "$@"; do
   f=$out/$arm-$i.json
   if [ -f $f ] && [ -z "$(python3 $here/valid.py $f)" ]; then continue; fi
   while pgrep -f "ss-tools/run.sh" >/dev/null || (( $(sysctl -n vm.loadavg | awk '{print int($2*10)}') > 50 )); do /bin/sleep 20; done
   unset SYNC_OFF SYNC_NOHOLD SYNC_NOFILTER
   case $arm in off) export SYNC_OFF=1;; nohold) export SYNC_NOHOLD=1;; nofilter) export SYNC_NOFILTER=1;; neither) export SYNC_NOHOLD=1 SYNC_NOFILTER=1;; esac
   viewers=desktop,browser; (( i % 2 == 0 )) && viewers=browser,desktop
   rm -f $f; env ${=ROW_ENV} $here/run.sh "$net" $f $viewers $secs > /dev/null
  done
 done
done
