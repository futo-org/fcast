#!/usr/bin/env bash
#
# Generate the DVB-subtitle MPEG-TS fixture the DVB tests demux.
#
# WHY A SCRIPT PLUS A CHECKED-IN FILE, not generation at test time: the shipping
# receiver's static GStreamer PINS OFF `dvbsubenc`, `mpegtsmux` and `mpegpsmux`
# as encoders/muxers (see xtask/src/gstreamer.rs), so the fixture can only ever
# be produced from a DEV GStreamer. Producing it once keeps the DVB tests
# deterministic and encoder-free wherever they run.
#
# Produces ~10 s of transport stream: one H.264 video track (so the file
# typefinds and demuxes like a real broadcast capture, and the subtitles have a
# timeline) plus one DVB subtitle track, one display set per second. The
# subtitle source is the pattern `dvbsubenc` documents - AYUV frames fully
# TRANSPARENT except where the subtitle is, so region detection has a bounding
# box - with `timeoverlay` making every set differ, which is what exercises
# region/CLUT updates across sets.
#
# Usage:  tools/make-dvb-fixture.sh [output.ts]
# Default output: ../fcast-sample-media/video/dvb_subtitles.ts
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/.." && pwd)"
out="${1:-$repo/../fcast-sample-media/video/dvb_subtitles.ts}"

# The dev GStreamer this repo's suites run against (the patched playback build).
# Sourcing it here rather than trusting the caller's shell keeps the fixture
# reproducible from a bare terminal.
if patched="$(cd "$repo" && cargo run -q -p xtask -- patched-plugins --quiet)"; then
  eval "$patched"
fi

need() {
  if ! gst-inspect-1.0 "$1" >/dev/null 2>&1; then
    echo "missing GStreamer element: $1" >&2
    return 1
  fi
}

need dvbsubenc
need mpegtsmux
need videotestsrc
need timeoverlay

# Whatever H.264/MPEG-2 encoder this build actually has. The video leg is
# scenery, so naming a fallback chain is cheaper than demanding one.
encoder=""
for candidate in "x264enc bitrate=512 speed-preset=ultrafast key-int-max=25" \
                 "openh264enc bitrate=512000" \
                 "avenc_mpeg2video bitrate=1000000"; do
  if gst-inspect-1.0 "${candidate%% *}" >/dev/null 2>&1; then
    encoder="$candidate"
    break
  fi
done
if [[ -z "$encoder" ]]; then
  echo "no usable video encoder (tried x264enc, openh264enc, avenc_mpeg2video)" >&2
  exit 1
fi
echo "video encoder: ${encoder%% *}"

mkdir -p "$(dirname "$out")"
tmp="$(mktemp "${out}.XXXXXX")"
trap 'rm -f "$tmp"' EXIT

set -x
gst-launch-1.0 -e \
  mpegtsmux name=mux ! filesink location="$tmp" \
  videotestsrc pattern=smpte num-buffers=250 is-live=false \
    ! video/x-raw,width=720,height=576,framerate=25/1 \
    ! videoconvert ! $encoder ! h264parse ! mux. \
  videotestsrc pattern=solid-color foreground-color=0x00000000 num-buffers=10 is-live=false \
    ! video/x-raw,format=AYUV,width=720,height=576,framerate=1/1 \
    ! timeoverlay halignment=center valignment=bottom font-desc="Sans Bold 36" \
    ! dvbsubenc max-colours=16 ! mux.
set +x

mv "$tmp" "$out"
trap - EXIT
ls -l "$out"
echo "wrote $out"
echo
echo "stream check:"
gst-discoverer-1.0 "$out" 2>&1 | sed -n '1,60p' || true
