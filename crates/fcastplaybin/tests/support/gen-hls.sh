#!/usr/bin/env bash
# HLS fixture for tests/regression_hls_codec_family.rs, master mixes codec
# families like YouTube. BANDWIDTH values are fiction so loopback ABR always
# tries the top variant.
set -euo pipefail

OUT="${1:-$(cd "$(dirname "$0")/../../../.." && pwd)/target/hls-fixtures}"

DURATION=6
FPS=15
SEG=2

RECIPE=v1-${DURATION}s-${FPS}fps-${SEG}s-h264ts-x2+vp9fmp4

if [ "$(cat "$OUT/.recipe" 2>/dev/null)" = "$RECIPE" ]; then
  echo "$OUT"
  exit 0
fi

command -v ffmpeg >/dev/null || { echo "gen-hls.sh: ffmpeg not found" >&2; exit 1; }

rm -rf "$OUT"
mkdir -p "$OUT/low" "$OUT/mid" "$OUT/vp9"

h264_variant() {
  local dir="$1" size="$2" rate="$3"
  ffmpeg -nostdin -y -hide_banner -loglevel error \
    -fflags +bitexact -flags +bitexact \
    -f lavfi -i "testsrc2=size=$size:rate=$FPS:duration=$DURATION" \
    -c:v libx264 -profile:v baseline -pix_fmt yuv420p \
    -b:v "$rate" \
    -g $((FPS * SEG)) -keyint_min $((FPS * SEG)) -sc_threshold 0 \
    -f hls -hls_time $SEG -hls_playlist_type vod -hls_list_size 0 \
    -hls_segment_type mpegts \
    -hls_segment_filename "$OUT/$dir/seg-%05d.ts" \
    "$OUT/$dir/media.m3u8"
}

h264_variant low 160x90 50k
h264_variant mid 320x180 150k

ffmpeg -nostdin -y -hide_banner -loglevel error \
  -fflags +bitexact -flags +bitexact \
  -f lavfi -i "testsrc2=size=320x180:rate=$FPS:duration=$DURATION" \
  -c:v libvpx-vp9 -pix_fmt yuv420p -b:v 200k \
  -g $((FPS * SEG)) -keyint_min $((FPS * SEG)) \
  -f hls -hls_time $SEG -hls_playlist_type vod -hls_list_size 0 \
  -hls_segment_type fmp4 \
  -hls_fmp4_init_filename "init.mp4" \
  -hls_segment_filename "$OUT/vp9/seg-%05d.m4s" \
  "$OUT/vp9/media.m3u8"

cat > "$OUT/master.m3u8" <<'EOF'
#EXTM3U
#EXT-X-INDEPENDENT-SEGMENTS
#EXT-X-STREAM-INF:BANDWIDTH=200000,CODECS="avc1.42C00B",RESOLUTION=160x90,FRAME-RATE=15
low/media.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=600000,CODECS="avc1.42C01E",RESOLUTION=320x180,FRAME-RATE=15
mid/media.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=4000000,CODECS="vp09.00.10.08",RESOLUTION=320x180,FRAME-RATE=15
vp9/media.m3u8
EOF

for f in low/media.m3u8 mid/media.m3u8 vp9/media.m3u8 vp9/init.mp4; do
  [ -s "$OUT/$f" ] || { echo "gen-hls.sh: missing $f" >&2; exit 1; }
done

echo "$RECIPE" > "$OUT/.recipe"
echo "$OUT"
