LOCAL_PATH := $(call my-dir)

include $(CLEAR_VARS)

LOCAL_MODULE := receiver_android
LOCAL_SHARED_LIBRARIES := gstreamer_android
LOCAL_SRC_FILES := dummy.cpp
LOCAL_LDLIBS := -llog -landroid
include $(BUILD_SHARED_LIBRARY)

ifndef GSTREAMER_ROOT_ANDROID
$(error GSTREAMER_ROOT_ANDROID is not defined!)
endif

ifeq ($(TARGET_ARCH_ABI),armeabi)
GSTREAMER_ROOT        := $(GSTREAMER_ROOT_ANDROID)/arm
else ifeq ($(TARGET_ARCH_ABI),armeabi-v7a)
GSTREAMER_ROOT        := $(GSTREAMER_ROOT_ANDROID)/armv7
else ifeq ($(TARGET_ARCH_ABI),arm64-v8a)
GSTREAMER_ROOT        := $(GSTREAMER_ROOT_ANDROID)/arm64
else ifeq ($(TARGET_ARCH_ABI),x86)
GSTREAMER_ROOT        := $(GSTREAMER_ROOT_ANDROID)/x86
else ifeq ($(TARGET_ARCH_ABI),x86_64)
GSTREAMER_ROOT        := $(GSTREAMER_ROOT_ANDROID)/x86_64
else
$(error Target arch ABI not supported: $(TARGET_ARCH_ABI))
endif

GSTREAMER_NDK_BUILD_PATH  := $(GSTREAMER_ROOT)/share/gst-android/ndk-build/
include $(GSTREAMER_NDK_BUILD_PATH)/plugins.mk

EFFECTS := audiofx deinterlace
GSTREAMER_PLUGINS_NET_NO_RSWEBRTC := tcp rtsp rtp rtpmanager udp dtls rist rtmp2 rtpmanagerbad rtponvif sctp sdpelem srtp srt webrtc nice mpegtslive quinn rsonvif raptorq rsrtp rsrtsp
CUSTOM_GSTREAMER_PLUGINS_CODECS := subparse ogg theora vorbis opus adaptivedemux2 alaw amrnb amrwbdec apetag audioparsers auparse avi dv flac flv flxdec icydemux id3demux isomp4 jpeg lame matroska mpg123 mulaw multipart png speex taglib vpx wavenc wavpack wavparse xingmux y4m adpcmdec adpcmenc assrender bz2 codecalpha codectimestamper dash dvbsubenc dvbsuboverlay dvdspu hls id3tag ivfparse midi mxf openh264 opusparse pcapparse pnm rfbsrc siren smoothstreaming subenc transcode videoparsersbad jpegformat gdp openjpeg spandsp sbc zbar rsvg svtav1 androidmedia rsaudioparsers cdg claxon dav1d rsclosedcaption ffv1 gif hsv isobmff lewton rav1e json rspng regex textwrap textahead

GSTREAMER_PLUGINS := $(GSTREAMER_PLUGINS_CORE) $(CUSTOM_GSTREAMER_PLUGINS_CODECS) $(GSTREAMER_PLUGINS_NET_NO_RSWEBRTC) $(GSTREAMER_PLUGINS_PLAYBACK) $(GSTREAMER_PLUGINS_CODECS_RESTRICTED) $(GSTREAMER_PLUGINS_SYS) $(EFFECTS)
GSTREAMER_EXTRA_DEPS      := gstreamer-video-1.0 gstreamer-gl-1.0 gstreamer-app-1.0 gstreamer-base-1.0 gstreamer-webrtc-1.0 gstreamer-pbutils-1.0 gstreamer-tag-1.0

include $(GSTREAMER_NDK_BUILD_PATH)/gstreamer-1.0.mk
