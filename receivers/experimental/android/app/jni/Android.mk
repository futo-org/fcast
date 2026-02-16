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

CORE := coreelements coretracers adder app audioconvert audiomixer audiorate audioresample gio pbtypes rawparse typefindfunctions videoconvertscale videorate volume autodetect videofilter insertbin mse switchbin fallbackswitch gopbuffer livesync rstracers streamgrouper threadshare
EFFECTS := audiofx deinterlace
GSTREAMER_PLUGINS_NET_NO_RSWEBRTC := tcp rtsp rtp rtpmanager udp dtls rist rtpmanagerbad rtponvif sctp sdpelem srtp srt webrtc nice mpegtslive quinn rsonvif raptorq rsrtp rsrtsp
CODECS := subparse ogg vorbis opus adaptivedemux2 alaw amrnb amrwbdec apetag audioparsers auparse avi flac flv flxdec icydemux id3demux isomp4 matroska mpg123 mulaw multipart png taglib vpx wavpack wavparse y4m assrender codecalpha codectimestamper dash dvbsuboverlay dvdspu hls ivfparse openh264 opusparse smoothstreaming videoparsersbad rsaudioparsers dav1d rsclosedcaption ffv1 gif isobmff androidmedia
CODECS_RESTRICTED := mpegtsdemux dvdsub libav
GSTREAMER_PLUGINS := $(CORE) $(CODECS) $(GSTREAMER_PLUGINS_NET_NO_RSWEBRTC) $(GSTREAMER_PLUGINS_PLAYBACK) $(CODECS_RESTRICTED) $(GSTREAMER_PLUGINS_SYS) $(EFFECTS)

GSTREAMER_EXTRA_DEPS      := gstreamer-video-1.0 gstreamer-gl-1.0 gstreamer-app-1.0 gstreamer-base-1.0 gstreamer-webrtc-1.0 gstreamer-pbutils-1.0 gstreamer-tag-1.0

include $(GSTREAMER_NDK_BUILD_PATH)/gstreamer-1.0.mk
