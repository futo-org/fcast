#!/usr/bin/env bash
#
# Generated with claude
#
# Cross-language interop test for the FCast v4 handshake proof-of-concepts.
#
# Runs each implementation's own selftest, then drives every
# sender <-> receiver combination across the C, Python and Node
# implementations (9 combinations) to confirm the v4 connection-setup
# (plaintext Version exchange + in-place TLS 1.3 upgrade + SPKI fingerprint
# pinning) is wire-compatible across languages.
#
# Requirements: a C compiler + OpenSSL dev (pkg-config openssl), python3 with
# `cryptography` (the local ./.venv is used if present), and node.
#
# Usage: ./crosstest.sh
set -u
cd "$(dirname "$0")"

PASS=0
FAIL=0
ok() { echo "PASS: $*"; PASS=$((PASS + 1)); }
no() { echo "FAIL: $*"; FAIL=$((FAIL + 1)); }

cleanup() { kill $(jobs -p) 2>/dev/null; }
trap cleanup EXIT

# --- build the C implementation -------------------------------------------
echo "== building C implementation =="
if cc -Wall -O2 v4_handshake.c -o v4_handshake_c \
    $(pkg-config --cflags --libs openssl) -lpthread; then
  echo "  built ./v4_handshake_c"
else
  echo "FAIL: C build failed"
  exit 1
fi

# --- implementation command prefixes --------------------------------------
PYBIN="./.venv/bin/python"
[ -x "$PYBIN" ] || PYBIN="python3"

names=(c py node)
declare -A cmd
cmd[c]="./v4_handshake_c"
cmd[py]="$PYBIN v4_handshake.py"
cmd[node]="node v4_handshake.js"

# Wait until a backgrounded receiver prints its fingerprint banner.
wait_fp() {
  local log="$1" fp=""
  for _ in $(seq 1 100); do
    fp=$(grep -m1 'fp (mDNS' "$log" 2>/dev/null | sed 's/.*: //')
    [ -n "$fp" ] && { printf '%s' "$fp"; return 0; }
    sleep 0.05
  done
  return 1
}

# --- per-implementation selftests -----------------------------------------
echo
echo "== selftests =="
for n in "${names[@]}"; do
  if ${cmd[$n]} selftest >"/tmp/v4_${n}_self.log" 2>&1; then
    ok "$n selftest"
  else
    no "$n selftest (see /tmp/v4_${n}_self.log)"
  fi
done

# --- full sender x receiver matrix ----------------------------------------
echo
echo "== cross matrix (sender -> receiver) =="
port=47010
for r in "${names[@]}"; do
  log="/tmp/v4_${r}_recv.log"
  : >"$log"
  ${cmd[$r]} receiver --host 127.0.0.1 --port "$port" >"$log" 2>&1 &
  rpid=$!
  if ! fp=$(wait_fp "$log"); then
    no "$r receiver did not start (see $log)"
    kill "$rpid" 2>/dev/null
    port=$((port + 1))
    continue
  fi
  for s in "${names[@]}"; do
    if ${cmd[$s]} sender --host 127.0.0.1 --port "$port" --fp "$fp" \
        >"/tmp/v4_${s}_to_${r}.log" 2>&1; then
      ok "$s sender -> $r receiver"
    else
      no "$s sender -> $r receiver (exit $?, see /tmp/v4_${s}_to_${r}.log)"
    fi
  done
  kill "$rpid" 2>/dev/null
  wait "$rpid" 2>/dev/null
  port=$((port + 1))
done

echo
echo "== summary: $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
