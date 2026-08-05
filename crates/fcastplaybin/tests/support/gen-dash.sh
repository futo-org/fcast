#!/usr/bin/env bash
# Generate the tiny local DASH VOD fixtures `tests/dash_testbed.rs` plays.
#
# Nothing here is checked in: the output lands under the workspace `target/`,
# which is gitignored, and the test regenerates it when missing. The encode is
# bitexact and driven from lavfi sources, so a regenerated tree is
# byte-identical and can be reused across runs.
#
#   tests/support/gen-dash.sh [outdir]   # default: <workspace>/target/dash-fixtures
#
# Layout:
#   <outdir>/vod/manifest.mpd        video + audio, NO text AdaptationSet
#   <outdir>/vod/manifest-text.mpd   the same media, plus an EMBEDDED text
#                                    AdaptationSet over vod/embedded.vtt
#   <outdir>/vod/embedded.vtt        reached ONLY through manifest-text.mpd
#   <outdir>/external/subs-a.vtt     the EXTERNAL subtitles: their own
#   <outdir>/external/subs-b.vtt     resources, named by no manifest, fetched
#                                    only because the caller attached them
#
# The subtitle files carry different cue text ("EMB"/"EXTA"/"EXTB") so the
# overlay tap can prove WHICH source rendered. Keeping the external ones in
# their own directory is deliberate: if a manifest referenced them, the
# external tests would really be testing an in-manifest track. The generator
# asserts that at the end.
#
# The embedded AdaptationSet is an unsegmented `text/vtt` Representation with a
# plain BaseURL, which is how DASH normally carries WebVTT and what
# `dashdemux2` exposes as a subtitle stream. ffmpeg's dash muxer cannot write
# WebVTT here at all ("Could not find tag for codec webvtt in stream #0" from
# the mp4 segment muxer, and `-c:s mov_text` would hand GStreamer tx3g rather
# than the WebVTT the receiver sees in the field), so it is spliced into the
# manifest rather than muxed.
set -euo pipefail

OUT="${1:-$(cd "$(dirname "$0")/../../../.." && pwd)/target/dash-fixtures}"

DURATION=90
FPS=15
SEG=2

# Bump when the recipe below changes, so a stale tree is rebuilt instead of
# silently reused.
RECIPE=v4-${DURATION}s-${FPS}fps-${SEG}s-160x90-50k-aac32k-1cue-emb+ext

if [ "$(cat "$OUT/.recipe" 2>/dev/null)" = "$RECIPE" ]; then
  echo "$OUT"
  exit 0
fi

command -v ffmpeg >/dev/null || { echo "gen-dash.sh: ffmpeg not found" >&2; exit 1; }

rm -rf "$OUT"
mkdir -p "$OUT/vod" "$OUT/external"

# One cue per second from t=0. The density is load-bearing: the races this
# testbed hunts need text in flight while the crate detaches or flushes the
# text branch, and a sparse track is not in flight often enough.
vtt() {
  local prefix="$1"
  printf 'WEBVTT\n\n'
  for i in $(seq 0 $((DURATION - 1))); do
    printf '%d\n%02d:%02d:%02d.000 --> %02d:%02d:%02d.900\n%s %02d\n\n' \
      "$((i + 1))" \
      $((i / 3600)) $((i / 60 % 60)) $((i % 60)) \
      $((i / 3600)) $((i / 60 % 60)) $((i % 60)) \
      "$prefix" "$i"
  done
}
vtt EMB > "$OUT/vod/embedded.vtt"
# Two externals, so a text-to-text switch with no "off" in between (the field
# sequence) can be told apart cue by cue. Prefixes are disjoint, not one a
# prefix of the other.
vtt EXTA > "$OUT/external/subs-a.vtt"
vtt EXTB > "$OUT/external/subs-b.vtt"

# `-fflags/-flags +bitexact` keeps encoder version strings out of the output so
# two generations compare equal. A keyframe every SEG seconds is what lets the
# muxer cut segments on GOP boundaries.
ffmpeg -nostdin -y -hide_banner -loglevel error \
  -fflags +bitexact -flags +bitexact \
  -f lavfi -i "testsrc2=size=160x90:rate=$FPS:duration=$DURATION" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=$DURATION" \
  -c:v libx264 -profile:v baseline -pix_fmt yuv420p \
  -b:v 50k -maxrate 60k -bufsize 120k \
  -g $((FPS * SEG)) -keyint_min $((FPS * SEG)) -sc_threshold 0 \
  -c:a aac -ac 1 -b:a 32k -ar 48000 \
  -adaptation_sets "id=0,streams=v id=1,streams=a" \
  -f dash -seg_duration $SEG -use_template 1 -use_timeline 0 \
  -init_seg_name 'init-$RepresentationID$.m4s' \
  -media_seg_name 'chunk-$RepresentationID$-$Number%05d$.m4s' \
  "$OUT/vod/manifest.mpd"

# Same media, one extra AdaptationSet. Spliced ahead of </Period>.
TEXT_AS=$(cat <<'XML'
		<AdaptationSet id="2" contentType="text" mimeType="text/vtt" lang="en">
			<Representation id="2" bandwidth="1000">
				<BaseURL>embedded.vtt</BaseURL>
			</Representation>
		</AdaptationSet>
XML
)
awk -v ins="$TEXT_AS" '
  /<\/Period>/ && !done { print ins; done = 1 }
  { print }
' "$OUT/vod/manifest.mpd" > "$OUT/vod/manifest-text.mpd"

grep -q 'text/vtt' "$OUT/vod/manifest-text.mpd" \
  || { echo "gen-dash.sh: failed to splice the text AdaptationSet" >&2; exit 1; }
# The external files must stay invisible to every manifest, or the external
# tests silently become in-manifest ones.
if grep -qE 'subs-[ab]\.vtt|external/' "$OUT/vod/"*.mpd; then
  echo "gen-dash.sh: a manifest references an external subtitle" >&2
  exit 1
fi

echo "$RECIPE" > "$OUT/.recipe"
echo "$OUT"
