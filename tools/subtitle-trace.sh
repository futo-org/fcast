#!/usr/bin/env bash
#
# Capture the trace that explains a DEAD EMBEDDED SUBTITLE TRACK in a real
# session, and nothing else.
#
# WHY THIS EXISTS
#
# The field reports embedded DASH subtitles that never appear, with one
# `Discarding data on subtitle_00: downstream returned FLUSHING` and a count
# that stays at 1. That shape is NOT reproducible off the owner's machine: the
# driver alone (dash_testbed::probe_default_subtitle_on_a_live_uri) and the
# full stack minus GUI (receiver-core player::tests::
# probe_default_subtitle_through_the_player, real FSink + real CueEngine +
# shipped parser autoplug) both render the owner's own stream correctly, five
# runs each. So the next occurrence has to carry its own evidence.
#
# The count staying at 1 is expected and is not the receiver undercounting:
# adaptivedemux2 sets `slot->warned_transient_flushing` on the first discard
# and clears it nowhere (gstadaptivedemux.c:3700-3701), so a slot that has
# latched downstream FLUSHING for good discards every later buffer SILENTLY.
# One warning is what a permanently dead track looks like.
#
# WHAT TO DO
#
#   1. tools/subtitle-trace.sh -- <receiver binary> [its args...]
#   2. Cast the item and let it run until the subtitles are visibly missing
#      (~90 s is plenty; the discard lands early).
#   3. Stop the receiver (Ctrl-C).
#   4. Send the file the script prints at the end. It is the EXTRACT, a few
#      hundred KB, not the whole log.
#
# THE ONE GOTCHA. `FCAST_LOG` is consulted only when the binary resolves NO
# log level of its own (receiver-core/src/lib.rs:172-182): a `--loglevel` flag
# or a `[log] level` in the config file wins, and this capture then comes back
# empty. The script refuses to run if it sees the flag, and warns about config.

set -u

PROFILE="light"
OUT="${FCAST_TRACE_DIR:-$PWD}/fcast-subtitle-trace-$(date +%Y%m%d-%H%M%S)"
CAP_MB="${FCAST_TRACE_CAP_MB:-512}"

usage() {
    cat <<'USAGE'
usage: subtitle-trace.sh [--deep] [--out DIR] -- <receiver binary> [args...]

  --deep     add per-buffer multiqueue logging. Answers "is the slot latched"
             directly, at roughly 10x the log volume. Use it for the SECOND
             capture if the first is inconclusive, not the first.
  --out DIR  where to write (default: $PWD).

env:
  FCAST_TRACE_CAP_MB   stop capturing past this many MB (default 512).
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --deep) PROFILE="deep"; shift ;;
        --out) OUT="$2/fcast-subtitle-trace-$(date +%Y%m%d-%H%M%S)"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        --) shift; break ;;
        *) echo "unknown option: $1" >&2; usage; exit 2 ;;
    esac
done

if [ $# -eq 0 ]; then
    echo "error: no receiver binary given (use -- <binary> [args])" >&2
    usage
    exit 2
fi

for arg in "$@"; do
    case "$arg" in
        --loglevel*|-l)
            echo "error: $arg makes the binary ignore FCAST_LOG, so this capture" >&2
            echo "       would contain no receiver logs. Drop it and re-run." >&2
            exit 2
            ;;
    esac
done
for cfg in "$HOME/.config/fcast/config.toml" "$HOME/.config/fcast-receiver/config.toml"; do
    if [ -f "$cfg" ] && grep -qE '^\s*level\s*=' "$cfg" 2>/dev/null; then
        echo "warning: $cfg sets [log] level, which overrides FCAST_LOG." >&2
        echo "         Comment it out or this capture will be empty." >&2
    fi
done

mkdir -p "$(dirname "$OUT")" || exit 1
RAW="$OUT.log"
EXTRACT="$OUT.extract.txt"

# The receiver side: the text branch's own decisions, plus the stall verdict.
# `warn` everywhere else keeps this readable and small.
export FCAST_LOG="warn,fcastplaybin=debug,receiver_core::player=debug,receiver_core::application=info"

# The GStreamer side. Level 4 (DEBUG) on the three elements that decide a text
# track's fate: who downloads and pushes it, who slots it, who queues it.
# GStreamer logging is routed through the same tracing subscriber
# (logging::init -> tracing_gstreamer::integrate_events), so GST_DEBUG_FILE
# does NOT apply and everything lands on stderr together, already interleaved.
if [ "$PROFILE" = "deep" ]; then
    export GST_DEBUG="2,adaptivedemux2:5,decodebin3:5,multiqueue:5"
else
    export GST_DEBUG="2,adaptivedemux2:4,decodebin3:4,multiqueue:4"
fi

echo "capturing to $RAW (profile: $PROFILE, cap ${CAP_MB}MB)"
echo "cast the item, wait until the subtitles are visibly missing, then Ctrl-C."
echo

# `head -c` bounds the file without killing the receiver early: it exits at the
# cap and the tee below simply stops having a reader.
"$@" 2>&1 | tee >(head -c $((CAP_MB * 1024 * 1024)) > "$RAW") >/dev/null
echo
echo "captured $(du -h "$RAW" 2>/dev/null | cut -f1) to $RAW"

# The extract: every line that decides the question, with context, so the whole
# log never has to leave the machine.
{
    echo "=== verdict / discards ==="
    grep -nE "Discarding data on|took a FLUSHING discard and has delivered nothing|keeps discarding data" "$RAW"
    echo
    echo "=== THE DISCRIMINATOR: upstream, or selection? ==="
    echo "# Read this section FIRST."
    echo "#"
    echo "# The text link loop refuses to build a branch for a routed stream"
    echo "# whose decodebin3 src pad carries no sticky CAPS, and that refusal is"
    echo "# silent and unbounded: the poll runs ~100x a second and says nothing,"
    echo "# while the selection reads as CONFIRMED and the track never appears."
    echo "# Measured at ~4025 refusals over 40 s on the reproduced case."
    echo "#"
    echo "# The escalation line below fires once per (stream, load) after the"
    echo "# grace period and carries the whole signature. Read it as:"
    echo "#"
    echo "#   sticky_stream_start=true sticky_segment=true, current_caps None"
    echo "#     -> UPSTREAM. Nothing ever traversed the stream's decodebin3"
    echo "#        slot. Selection is fine; look at the slot and at whatever"
    echo "#        feeds it (the seeding lines below, adaptivedemux2 discards)."
    echo "#   no escalation line at all, but a track that never renders"
    echo "#     -> NOT this gate. Look at selection and at the branch's life."
    echo "#"
    echo "# The slot-seeding lines are the external-subtitle half of the same"
    echo "# story: a refused GAP used to leave a stream permanently slotless,"
    echo "# which is one way to arrive at exactly the signature above."
    grep -nE "has carried no CAPS for the whole grace period|are not subtitles the renderer can carry|no text branch joined its consumer tail|seeding a decodebin3 slot|refused the slot-seeding gap|input stream drained" "$RAW" | head -60
    echo
    echo "=== the text branch's life ==="
    grep -nE "text stream joined its consumer tail|parked text stream|reclaiming the text slot|following the data|disposing of a text branch|could not be wired|SubtitleTrackUnsupported|no routed text pad" "$RAW"
    echo
    echo "=== selection ==="
    grep -nE "sent SELECT_STREAMS|SELECT_STREAMS event refused|a selection was refused|a selection was skipped|deferring the re-emit flush|Refresh seek|rolling back a refused selection" "$RAW"
    echo
    echo "=== decodebin3 slots and outputs ==="
    grep -nE "Creating new slot|Created new slot|Reconfiguring output|Re-using existing unused slot|db_output_stream_new" "$RAW"
    echo
    echo "=== multiqueue flushes (a latch clears only on FLUSH_STOP) ==="
    grep -nE "gst_single_queue_flush|Received flush (start|stop) event|srcresult" "$RAW"
    echo
    echo "=== errors and warnings ==="
    grep -nE "^\S+\s+(ERROR|WARN)|GStreamer-WARNING|Internal data stream error" "$RAW" | head -200
} > "$EXTRACT" 2>/dev/null

echo "extract: $EXTRACT  ($(wc -l < "$EXTRACT" 2>/dev/null) lines)"
echo "send the extract. Keep $RAW until the extract has been read."
