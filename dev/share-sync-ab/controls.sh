#!/bin/bash
# Behaviour-neutral byte controls over the full drop sweep, for the noise bounds of
# `drop_sweep_stays_within_mains_noise` (sim.rs). Run on a clean worktree of main with its own target:
#   git worktree add --detach /tmp/ss213-ctrl origin/main
#   CTRL_TREE=/tmp/ss213-ctrl CARGO_TARGET_DIR=/tmp/ss213-ctarget dev/share-sync-ab/controls.sh
# Writes $SS_WORK/sweep-<name>.txt (one SWEEP line per case); compare with sweepcmp.py.
# Never share a target dir between worktrees: cargo then reuses the other tree's test binary.
set -u
cd "${CTRL_TREE:?}"
export CARGO_BUILD_JOBS=4
out=${SS_WORK:-/tmp/ss213}; mkdir -p "$out"
f=vendor/str0m/src/streams/send.rs
orig='        s.values.push((SdesType::CNAME, cname.to_string()));'
run() {
  git checkout -q -- $f
  python3 - "$f" "$orig" "$2" <<'PY'
import sys; p,o,n=sys.argv[1:]; s=open(p).read(); assert s.count(o)==1; open(p,'w').write(s.replace(o,n))
PY
  touch $f
  nice -n 10 cargo test --locked -p gwar-server --lib drop_sweep_stays_within_mains_noise -- --nocapture 2>&1 | grep -E "^SWEEP s" > "$out/sweep-$1.txt"
  echo "$1 $(wc -l < "$out/sweep-$1.txt")"
}
run c1  '        s.values.push((SdesType::CNAME, format!("{cname}xxxx")));'
run c2  '        s.values.push((SdesType::CNAME, cname.to_string())); s.values.push((SdesType::NAME, "g".into()));'
run c3  '        s.values.push((SdesType::CNAME, format!("{cname}xxxxxxxxxxxx")));'
run c4  '        s.values.push((SdesType::CNAME, cname[..16].to_string()));'
run c5  '        s.values.push((SdesType::CNAME, cname[..8].to_string()));'
run c6  '        s.values.push((SdesType::CNAME, cname[..1].to_string()));'
run c7  '        s.values.push((SdesType::CNAME, format!("{cname}xxxxxxxx")));'
run c8  '        s.values.push((SdesType::CNAME, format!("{cname}xxxxxxxxxxxxxxxx")));'
run c9  '        s.values.push((SdesType::CNAME, cname.to_string())); s.values.push((SdesType::NOTE, "xxxxxxxxxxxxxxxxxxxxxxxxxx".into()));'
run c10 '        s.values.push((SdesType::CNAME, cname[..12].to_string()));'
git checkout -q -- $f
