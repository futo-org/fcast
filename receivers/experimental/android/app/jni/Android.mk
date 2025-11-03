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

# GSTREAMER_PLUGINS         := $(GSTREAMER_PLUGINS_CORE) $(GSTREAMER_PLUGINS_NET) $(GSTREAMER_PLUGINS_CODECS) $(GSTREAMER_PLUGINS_CODECS_RESTRICTED) $(GSTREAMER_PLUGINS_SYS) $(GSTREAMER_PLUGINS_PLAYBACK)
# GSTREAMER_PLUGINS         := $(GSTREAMER_PLUGINS_CORE) $(GSTREAMER_PLUGINS_PLAYBACK)
CORE := coreelements coretracers adder app audioconvert audiomixer audiorate audioresample audiotestsrc compositor gio overlaycomposition pango rawparse typefindfunctions videoconvertscale videorate videotestsrc volume autodetect videofilter insertbin switchbin fallbackswitch gopbuffer livesync originalbuffer rsinter rstracers streamgrouper threadshare togglerecord
CODECS := subparse ogg theora vorbis opus adaptivedemux2 alaw amrnb amrwbdec apetag audioparsers auparse avi dv flac flv flxdec icydemux id3demux isomp4 jpeg lame matroska mpg123 mulaw multipart png speex taglib vpx wavenc wavpack wavparse xingmux y4menc adpcmdec adpcmenc assrender bz2 codecalpha codectimestamper dash dvbsubenc dvbsuboverlay dvdspu hls id3tag ivfparse midi mxf openh264 opusparse pcapparse pnm rfbsrc siren smoothstreaming subenc transcode videoparsersbad y4mdec jpegformat gdp openjpeg spandsp sbc zbar rsvg svtav1 x265 androidmedia cdg claxon dav1d rsclosedcaption ffv1 fmp4 mp4 gif hsv lewton rav1e json rspng regex textwrap textahead
# NET := tcp rtsp rtp rtpmanager soup udp dtls netsim rist rtmp2 rtpmanagerbad rtponvif sctp sdpelem srtp srt webrtc nice rtspclientsink aws hlssink3 hlsmultivariantsink mpegtslive ndi quinn rsonvif raptorq rsrelationmeta reqwest rsrtp rsrtsp webrtchttp rswebrtc
NET := tcp rtsp rtp rtpmanager soup udp dtls netsim rist rtmp2 rtpmanagerbad rtponvif sctp sdpelem srtp srt webrtc nice rtspclientsink aws hlssink3 hlsmultivariantsink mpegtslive ndi quinn rsonvif raptorq reqwest rsrtp rsrtsp webrtchttp rswebrtc
GSTREAMER_PLUGINS         := $(CORE) $(CODECS) $(GSTREAMER_PLUGINS_PLAYBACK) $(NET)

# GSTREAMER_EXTRA_DEPS      := gstreamer-video-1.0 glib-2.0 gstreamer-app-1.0 gstreamer-base-1.0 gstreamer-webrtc-1.0
GSTREAMER_EXTRA_DEPS      := gstreamer-video-1.0 gstreamer-gl-1.0 gstreamer-app-1.0 gstreamer-base-1.0 gstreamer-webrtc-1.0

G_IO_MODULES = openssl

include $(GSTREAMER_NDK_BUILD_PATH)/gstreamer-1.0.mk
