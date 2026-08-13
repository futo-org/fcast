#!/usr/bin/env bash
# Run fcastplaybin's fuzz drivers under ThreadSanitizer.
#
# GStreamer and GLib come from the nix store uninstrumented, so TSan cannot see
# their happens-before edges: a pad probe serialized by an object lock, a stream
# lock, a GLib GMutex - all look like two threads touching bytes for no reason.
# The only believable report is one with NO GStreamer/GLib frame ANYWHERE in it,
# and that is what this script counts; reports that do name them are shown and
# counted rather than declared clean. For a specific recurring C-internal shape
# the answer is a narrow, commented suppression - never a blanket `race:*`.
#
# THE CANARIES COME FIRST. tests/tsan_canary.rs holds a deliberate
# unsynchronized write pair (must report) and a parking_lot-synchronized pair
# (must stay silent). Until both behave, a green fuzz run means nothing.
#
# Usage:
#   tools/run-tsan.sh                       # canaries, then both fuzz drivers
#   tools/run-tsan.sh fuzz_buffering        # canaries, then that target only
#   tools/run-tsan.sh fcast-video:engine_stress   # a target in another package
#   tools/run-tsan.sh --canaries-only       # just the instrument check
#   tools/run-tsan.sh --skip-canaries fuzz_scenarios
#   FCAST_NO_HANDS=1 tools/run-tsan.sh      # every FCAST_* var is passed through
#   TSAN_FILTER='a_specific_test' tools/run-tsan.sh fuzz_buffering
#
# COST: the instrumented build is a full -Zbuild-std rebuild of std and every
# dependency, and instrumented execution is ~10-20x slower.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
CRATE="$REPO/crates/fcastplaybin"
SUPPRESSIONS="$CRATE/tsan-suppressions.txt"
# Its own target dir: instrumented artifacts share nothing with the ordinary
# build, and mixing them only costs full rebuilds in both directions.
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO/target-tsan}"
TRIPLE="${TSAN_TARGET:-x86_64-unknown-linux-gnu}"
# -Zbuild-std needs the standard library's SOURCE, because std itself has to be
# compiled with the sanitizer; an uninstrumented std is a blind spot.
JOBS="${TSAN_JOBS:-4}"
# Plain `cargo`, not `cargo +nightly`: the devshell has no rustup and its cargo
# is the nightly the sanitizer needs (rust-src in the sysroot for -Zbuild-std).
# Override on a rustup checkout: CARGO="cargo +nightly" tools/run-tsan.sh
CARGO="${CARGO:-cargo}"

CANARIES=1
CANARIES_ONLY=0
TARGETS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --skip-canaries) CANARIES=0 ;;
    --canaries-only) CANARIES_ONLY=1 ;;
    -h|--help) sed -n '2,45p' "$0"; exit 0 ;;
    -*) echo "unknown option: $1" >&2; exit 2 ;;
    *) TARGETS+=("$1") ;;
  esac
  shift
done
if [ ${#TARGETS[@]} -eq 0 ]; then
  TARGETS=(fuzz_scenarios fuzz_buffering)
fi

if [ ! -f "$SUPPRESSIONS" ]; then
  echo "missing $SUPPRESSIONS" >&2
  exit 1
fi

# The same patched GStreamer the ordinary suite runs against, or the drivers
# exercise a configuration the product never ships (see
# xtask/src/patched_plugins.rs). Best-effort: unpatched is still a valid run.
# Before the sanitizer RUSTFLAGS below, so xtask itself builds uninstrumented.
if [ -z "${GST_PLUGIN_PATH:-}" ]; then
  if PATCHED="$(cd "$REPO" && cargo run -q -p xtask -- patched-plugins --quiet 2>/dev/null)"; then
    eval "$PATCHED"
  else
    echo ">> WARNING: no patched playback plugin; running against the devshell's GStreamer" >&2
  fi
fi

export CARGO_TARGET_DIR="$TARGET_DIR"
export RUSTFLAGS="${RUSTFLAGS:-} -Zsanitizer=thread"
# history_size=7 keeps the OTHER access's stack alive on long runs (a one-sided
# report can't be triaged); second_deadlock_stack=1 does it for lock-order ones.
TSAN_BASE="suppressions=$SUPPRESSIONS history_size=7 second_deadlock_stack=1"

# A target may name its package: "fcast-video:engine_stress". Bare targets are
# fcastplaybin's, which is where every driver lived until phase 6 put the
# subtitle engine's two workers under load in a crate of their own.
run_cargo() {
  local spec="$1" package test_target
  shift
  case "$spec" in
    *:*) package="${spec%%:*}"; test_target="${spec#*:}" ;;
    *)   package="fcastplaybin"; test_target="$spec" ;;
  esac
  # Unquoted on purpose: CARGO may carry a toolchain word ("cargo +nightly").
  # shellcheck disable=SC2086
  $CARGO test -Zbuild-std --target "$TRIPLE" -j "$JOBS" \
    -p "$package" --test "$test_target" "$@"
}

canary() {
  local name="$1" expectation="$2" log
  log="$(mktemp -t "tsan-canary-$name.XXXXXX")"
  # exitcode=0: the canary's verdict is read from the REPORT, not from a process
  # status, and the positive one is meant to race.
  if ! TSAN_OPTIONS="$TSAN_BASE exitcode=0" \
      run_cargo tsan_canary -- --ignored --exact "$name" --test-threads=1 \
      >"$log" 2>&1; then
    echo ">> the canary target failed to build or run; see $log" >&2
    tail -40 "$log" >&2
    exit 1
  fi
  local reported=0
  grep -q "ThreadSanitizer: data race" "$log" && reported=1
  if [ "$reported" != "$expectation" ]; then
    echo ">> CANARY FAILED: $name reported=$reported expected=$expectation (log: $log)" >&2
    echo ">> The instrument is not measuring what the gate claims it measures." >&2
    exit 1
  fi
  echo ">> canary ok: $name (data race reported: $reported)"
  rm -f "$log"
}

if [ "$CANARIES" = 1 ]; then
  echo ">> validating the instrument (tests/tsan_canary.rs)"
  canary tsan_canary_positive 1
  canary tsan_canary_negative 0
fi
if [ "$CANARIES_ONLY" = 1 ]; then
  exit 0
fi

# One report block per WARNING. A block with NO GStreamer/GLib frame anywhere is
# between two instrumented Rust accesses with nothing uninstrumented in between,
# so its happens-before edge is the crate's own and TSan can be believed. One
# that names those libraries may still be real, but nothing here can prove it,
# so it is counted and set aside rather than declared clean.
UNINSTRUMENTED='libgstreamer-|libglib-|libgobject-|libgio-|libgst[a-z0-9]*-'

triage() {
  local log="$1"
  awk -v pat="$UNINSTRUMENTED" '
    /^WARNING: ThreadSanitizer:/ { inblock = 1; block = ""; c = 0 }
    inblock {
      block = block $0 "\n"
      if ($0 ~ pat) c++
      if ($0 ~ /^SUMMARY: ThreadSanitizer:/) {
        total++
        if (c == 0) { real++; printf "%s\n", block }
        inblock = 0
      }
    }
    END { printf "TRIAGE %d %d\n", total, real }
  ' "$log"
}

status=0
for target in "${TARGETS[@]}"; do
  echo ">> TSan: $target"
  log="$(mktemp -t "tsan-$target.XXXXXX")"
  # --include-ignored is load-bearing: BOTH fuzz drivers are `#[ignore]`d, so a
  # plain run of these targets sanitizes nothing at all. --test-threads=1 so a
  # report names ONE test and the slowdown isn't multiplied by the core count.
  # exitcode=0 because the process status cannot carry the verdict (with
  # uninstrumented GStreamer there are always reports); test failures are read
  # out of the harness line instead, so a wedge or a panic still fails.
  TSAN_OPTIONS="$TSAN_BASE exitcode=0" \
    run_cargo "$target" -- --test-threads=1 --include-ignored \
      ${TSAN_FILTER:+"$TSAN_FILTER"} 2>&1 | tee "$log"

  if grep -qE "^test result: FAILED|error: test failed" "$log" 2>/dev/null; then
    status=1
    echo ">> TSan: $target FAILED its own tests" >&2
  fi
  triaged="$(triage "$log")"
  # The believable reports in full, then the counts. `|| true` on both: an empty
  # match is the GOOD outcome here, and grep says that with status 1.
  printf '%s\n' "$triaged" | grep -v '^TRIAGE ' >&2 || true
  verdict="$(printf '%s\n' "$triaged" | grep '^TRIAGE ' || true)"
  total="$(echo "$verdict" | awk '{print $2}')"
  real="$(echo "$verdict" | awk '{print $3}')"
  echo ">> TSan: $target - $total report(s), $real with a purely crate-Rust stack"
  if [ "${real:-0}" -gt 0 ]; then
    status=1
    echo ">> TSan: $target FAILED: $real report(s) TSan can be believed about" >&2
  else
    echo ">> TSan: $target clean by the gate's definition (log: $log)"
  fi
done
exit $status
