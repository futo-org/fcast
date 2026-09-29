#!/usr/bin/env bash
# Assembles the shared compression dictionary from what the receiver can say:
# GStreamer's error texts and enum names, the codec description table the
# missing-plugin message is built from, the container types, the decoder
# factory names, and the receiver's and flapjack's message templates. Least
# likely first, likeliest last, where a back-reference is cheapest.
#
#   dict/generate.sh <gstreamer checkout> > dict/dictionary.bin
#
# Encoder and decoder must share the file byte for byte: a blob is read by
# the decoder built from the same tree.
set -euo pipefail
gst=${1:?path to the gstreamer monorepo checkout}
desc=$gst/subprojects/gst-plugins-base/gst-libs/gst/pbutils/descriptions.c
err=$gst/subprojects/gstreamer/gst/gsterror.c

# codec descriptions, as "<name> decoder"
grep -oE '\{"(video|audio|image|application|subtitle|text)/[^"]+",\s*(N_\()?"[^"]+"' "$desc" \
    | sed -E 's/\{"([^"]+)",\s*(N_\()?"([^"]+)"/\3 decoder\n/' | sort -u | tr -d '\n'
echo
# gstreamer's error texts
grep -hoE 'N_ \("[^"]+"\)|_\("[^"]+"\)' "$err" | sed 's/^N_ (//;s/^_(//;s/)$//;s/"//g' | grep -v '%s' | tr '\n' ' '
echo
# the domain and code names flapjack prints in brackets
printf '%s' "Failed TooLazy NotImplemented TypeNotFound WrongType CodecNotFound Decode Encode Demux Mux Format Decrypt DecryptNokey NotFound Busy OpenRead OpenWrite OpenReadWrite Close Read Write Seek Sync Settings NoSpaceLeft NotAuthorized StateChange Pad Thread Negotiation Event Caps Tag MissingPlugin Clock Disabled Init Shutdown [library  [core  [resource  [stream "
echo
# decoder factory names: the static build's ffmpeg set, then hardware and android
for c in h264 hevc vp8 vp9 av1 mpeg2video mpeg4 h263 mjpeg prores vc1 theora aac ac3 eac3 mp3 dca opus vorbis flac alac truehd; do printf 'avdec_%s ' "$c"; done
printf '%s' "vah264dec vah265dec vaav1dec vavp9dec vavp8dec vajpegdec v4l2h264dec v4l2h265dec nvh264dec nvh265dec openh264dec dav1ddec vp8dec vp9dec av1dec theoradec jpegdec pngdec amcviddec-omxgoogleh264decoder amcviddec-c2androidavcdecoder amcviddec-c2androidhevcdecoder amcviddec-c2androidvp9decoder amcviddec-c2androidav1decoder amcviddec-omxamlogicavcdecoder amcviddec-omxamlogichevcdecoder c2.android.avc.decoder c2.android.hevc.decoder c2.amlogic.avc.decoder "
echo
# container and content types
printf '%s' "application/dash+xml application/vnd.apple.mpegurl application/x-mpegurl application/x-hls application/x-sabr-ump application/x-fwebrtc application/x-whep audio/mpegurl audio/mpeg audio/mp4 audio/flac audio/ogg audio/x-matroska audio/webm image/jpeg image/png image/webp image/gif video/webm video/x-msvideo video/quicktime video/mp2t video/x-matroska video/mp4 "
echo
# hosts, senders and devices seen so far
printf '%s' "googlevideo.com youtube.com http:// https:// 192.168. .local file:// Grayjay FCast Sender SDK v0. FCast Sender Chromecast Android TV "
echo
# the receiver's and flapjack's own templates, likeliest last
printf '%s' "Your GStreamer installation is missing a plug-in. Could not open resource for reading. Could not decode stream. Internal data stream error. streaming stopped, reason not-negotiated (-4) reason error (-5) Could not read from resource. Resource not found. Server does not support seeking. failed to reach Paused failed to reach Playing no decoder for  (from  the pipeline is descending below the crate loading the media failed:  ; the receiver's reload is disabled ; reloading the item did not clear it either  (uri  [stream CodecNotFound] [stream Decode] [stream Failed] [resource NotFound] [resource Read] [resource OpenRead] [core Negotiation] There is no codec present that can handle the stream's type. no stream of this item could be decoded "
