#!/usr/bin/env bash
# Stage-aware corpus harness.
#
# Each fixture declares its contract in header comments:
#   // EXPECT:  DEADLOCK | NO_DEADLOCK
#   // CYCLE:   space-separated lock names that must all appear in one cycle
#   // MINSTAGE: smallest implementation stage at which this should pass
#
# Usage: run_corpus.sh [STAGE]   (default STAGE=0)
# Fixtures with MINSTAGE > STAGE are reported PENDING (run but not asserted).
set -u
STAGE="${1:-0}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CORPUS="$ROOT/tests/corpus"
BIN="$ROOT/target/debug/lockdex"
D8="${LOCKDEX_D8:-/home/zim/dev/aosp/out/host/linux-x86/bin/d8}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

export CARGO_HOME="${CARGO_HOME:-/mnt/agent/tmp/cargo}"
( cd "$ROOT" && cargo build -q ) || { echo "build failed"; exit 1; }

pass=0; fail=0; pending=0
hdr() { grep -m1 "^// $1:" "$2" | sed "s|^// $1:||" | xargs; }

for f in "$CORPUS"/*.java; do
  name="$(basename "$f" .java)"
  expect="$(hdr EXPECT "$f")"
  cycle="$(hdr CYCLE "$f")"
  minstage="$(hdr MINSTAGE "$f")"; minstage="${minstage:-0}"

  cd="$WORK/$name"; mkdir -p "$cd/classes"
  if ! javac -d "$cd/classes" "$f" 2>"$cd/javac.log"; then
    echo "FAIL  $name (javac error)"; cat "$cd/javac.log"; fail=$((fail+1)); continue
  fi
  if ! "$D8" --min-api 26 --output "$cd" $(find "$cd/classes" -name '*.class') 2>"$cd/d8.log"; then
    echo "FAIL  $name (d8 error)"; cat "$cd/d8.log"; fail=$((fail+1)); continue
  fi
  json="$("$BIN" analyze "$cd/classes.dex" --format json 2>"$cd/run.log")" || {
    echo "FAIL  $name (lockdex error)"; cat "$cd/run.log"; fail=$((fail+1)); continue; }

  verdict="$(EXPECT="$expect" CYCLE="$cycle" python3 - "$json" <<'PY'
import json, os, sys
rep = json.loads(sys.argv[1])
expect = os.environ["EXPECT"].strip()
cyc = os.environ["CYCLE"].split()
cycles = rep.get("cycles", [])
def has_cycle(locks):
    return any(set(locks) <= set(c["locks"]) for c in cycles)
if expect == "DEADLOCK":
    ok = has_cycle(cyc) if cyc else len(cycles) > 0
elif expect == "NO_DEADLOCK":
    # if a specific cycle was named, only that one must be absent; otherwise no cycles at all
    ok = (not has_cycle(cyc)) if cyc else len(cycles) == 0
else:
    print("BADSPEC"); sys.exit()
print("OK" if ok else f"WRONG (sccs={rep.get('scc_count')}, cycles={[c['locks'] for c in cycles]})")
PY
)"

  if [ "$minstage" -gt "$STAGE" ]; then
    echo "PEND  $name (minstage=$minstage)  -> $verdict"
    pending=$((pending+1))
  elif [ "$verdict" = "OK" ]; then
    echo "PASS  $name"
    pass=$((pass+1))
  else
    echo "FAIL  $name  expect=$expect  $verdict"
    fail=$((fail+1))
  fi
done

echo "----"
echo "stage $STAGE: $pass passed, $fail failed, $pending pending"
[ "$fail" -eq 0 ]
