LOCAL_PATH := $(call my-dir)

include $(CLEAR_VARS)

LOCAL_MODULE    := android_sender
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

GSTREAMER_PLUGINS_CORE_CUSTOM := coreelements app audioconvert audiorate audioresample videorate videoconvertscale videofilter videoparsersbad
GSTREAMER_PLUGINS_NET_CUSTOM := tcp rtpmanager udp srtp dtls nice webrtc rtp rsrtp
GSTREAMER_PLUGINS_CODECS_CUSTOM := vpx
GSTREAMER_PLUGINS         := $(GSTREAMER_PLUGINS_CORE_CUSTOM) $(GSTREAMER_PLUGINS_CODECS_CUSTOM) $(GSTREAMER_PLUGINS_NET_CUSTOM) \

GSTREAMER_EXTRA_DEPS      := gstreamer-video-1.0 gstreamer-audio-1.0 glib-2.0 gstreamer-app-1.0 gstreamer-base-1.0 gstreamer-webrtc-1.0 gstreamer-rtp-1.0

G_IO_MODULES = openssl

include $(GSTREAMER_NDK_BUILD_PATH)/gstreamer-1.0.mk
