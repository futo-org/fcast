#!/usr/bin/env sh

export OMAS_PROJECT_ROOT="$(pwd)"

if [[ ! $OMAS_PROJECT_ROOT == */OpenMirroring/receiver-android ]]
then
    echo "ERROR: Make sure to execute this script in the receiver-android directory"
    exit 1
fi

if [[ ! -v GSTREAMER_ROOT_ANDROID ]]
then
    echo "ERROR: GSTREAMER_ROOT_ANDROID is not set"
    exit 1
fi

mkdir "../target"

cd "../target/"

export BUILD_SYSTEM="$ANDROID_NDK_ROOT/build/core"
export GSTREAMER_JAVA_SRC_DIR="../receiver-android/app/src/main/java"
export NDK_PROJECT_PATH="../receiver-android/app/"
export GSTREAMER_NDK_BUILD_PATH="$GSTREAMER_ROOT_ANDROID/share/gst-android/ndk-build"

set -xe

make -f "$ANDROID_NDK_ROOT/build/core/build-local.mk"
