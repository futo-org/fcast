#!/bin/bash
# Generates the media the FAST conformance suite expects in its sample directory, for use when the
# upstream fcast-sample-media repository is not available. The files carry the names FAST asks
# for; their content is synthetic (test pattern and tone).
#
# usage: make-sample-media.sh <output-dir>
set -euo pipefail

out="${1:?usage: make-sample-media.sh <output-dir>}"
mkdir -p "$out/image" "$out/video" "$out/audio" "$out/subs"

ff() {
    ffmpeg -hide_banner -loglevel error -y "$@"
}

video() { # <seconds> <output> [extra ffmpeg output options]
    local seconds="$1" output="$2"
    shift 2
    ff -f lavfi -i "testsrc2=size=320x180:rate=10:duration=$seconds" \
       -f lavfi -i "sine=frequency=440:duration=$seconds" \
       -c:v libx264 -profile:v baseline -pix_fmt yuv420p -g 20 -b:v 80k \
       -c:a aac -b:a 48k -ac 2 "$@" "$output"
}

subtitle() { # <language word> <output>
    local word="$1" output="$2" i
    : > "$output"
    for i in $(seq 0 119); do
        printf '%d\n%02d:%02d:%02d,000 --> %02d:%02d:%02d,500\n%s %d\n\n' \
            $((i + 1)) 0 $((i * 2 / 60)) $((i * 2 % 60)) 0 $((i * 2 / 60)) $((i * 2 % 60 + 1)) "$word" $((i + 1)) >> "$output"
    done
}

ff -f lavfi -i "testsrc2=size=640x360" -frames:v 1 "$out/image/flowers.jpg"
ff -f lavfi -i "smptebars=size=640x360" -frames:v 1 "$out/image/garden.jpg"
ff -f lavfi -i "testsrc2=size=160x90:rate=5:duration=2" "$out/image/animated.gif"

video 240 "$out/video/BigBuckBunny.mp4" -movflags +faststart
video 2 "$out/video/short_clip.mkv"
video 45 "$out/video/clip_45s.mkv"

subtitle "Line" "$out/subs/sample_en.srt"
subtitle "Linea" "$out/subs/sample_es.srt"

# A WebVTT file larger than one 512 KiB packet, so that a companion transfer needs several parts.
{
    printf 'WEBVTT\n\n'
    for i in $(seq 0 17999); do
        printf '%02d:%02d:%02d.%03d --> %02d:%02d:%02d.%03d\nDense cue number %d\n\n' \
            $((i / 36000)) $((i / 600 % 60)) $((i / 10 % 60)) $((i % 10 * 100)) \
            $((i / 36000)) $((i / 600 % 60)) $((i / 10 % 60)) $((i % 10 * 100 + 90)) $i
    done
} > "$out/subs/generated_dense.vtt"

# Multi-track Matroska files. A browser player cannot enumerate their embedded tracks, so the
# FAST cases that use them are listed as not applicable; the files exist so FAST can serve them.
ff -i "$out/video/BigBuckBunny.mp4" -i "$out/subs/sample_en.srt" -i "$out/subs/sample_es.srt" -i "$out/subs/sample_en.srt" \
   -map 0:v -map 0:a -map 1 -map 2 -map 3 -c:v copy -c:a copy -c:s srt -t 180 "$out/video/video_with_subs.mkv"
ff -i "$out/video/BigBuckBunny.mp4" -i "$out/subs/sample_en.srt" -i "$out/subs/sample_es.srt" \
   -map 0:v -map 0:a -map 0:a -map 1 -map 2 -c:v copy -c:a copy -c:s srt -t 60 "$out/video/video_multi_track.mkv"
ff -i "$out/video/BigBuckBunny.mp4" -map 0:v -map 0:v -map 0:a -c copy -t 60 "$out/video/video_dual_video.mkv"
cp "$out/video/video_with_subs.mkv" "$out/video/video_with_vobsub.mkv"

ff -f lavfi -i "sine=frequency=330:duration=250" -c:a libmp3lame -b:a 64k "$out/audio/Court_House_Blues_Take_1.mp3"
ff -f lavfi -i "sine=frequency=550:duration=60" -c:a libmp3lame -b:a 64k "$out/audio/Dont_Go_Way_Nobody.mp3"

echo "sample media written to $out"
