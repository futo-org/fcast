#!/usr/bin/env bash
# Casts the first tracks of an album as an autoplay queue and seeks each one
# to its last seconds, so every boundary is heard within a minute.
#
#   gapless-album.sh <receiver-ip> <album-dir> [count] [fast gapless options]
#
# Options after the count go to `fast gapless`: --tail-secs, --dwell-secs and
# --strict, which fails the run when a boundary reports Ended or Idle.
set -euo pipefail

if [ $# -lt 2 ]; then
    sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
fi
host=$1
album=$2
count=${3:-4}
shift $(( $# < 3 ? $# : 3 ))

tracks=()
while IFS= read -r -d '' track; do
    tracks+=("$track")
done < <(find "$album" -maxdepth 1 -type f \
    \( -iname '*.mp3' -o -iname '*.flac' -o -iname '*.m4a' -o -iname '*.opus' \
    -o -iname '*.ogg' -o -iname '*.wav' \) -print0 | sort -z | head -z -n "$count")

if [ ${#tracks[@]} -eq 0 ]; then
    echo "no audio files in $album" >&2
    exit 1
fi

cd "$(dirname "$0")/../.."
exec cargo fast -H "$host" gapless "$@" "${tracks[@]}"
