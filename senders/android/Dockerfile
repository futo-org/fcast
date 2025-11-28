FROM rust:1-trixie

RUN apt update && apt install -y build-essential cmake pkg-config automake autoconf libtool \
  libprotobuf-dev protobuf-compiler wget2 openjdk-21-jdk xz-utils curl

RUN cargo install cargo-ndk

RUN rustup target add aarch64-linux-android \
    armv7-linux-androideabi \
    x86_64-linux-android \
    i686-linux-android \
    x86_64-unknown-linux-gnu

RUN mkdir -p /thirdparty

RUN wget2 https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip -O /thirdparty/commandlinetools-linux-11076708_latest.zip
RUN unzip /thirdparty/commandlinetools-linux-11076708_latest.zip -d /thirdparty/
RUN rm /thirdparty/commandlinetools-linux-11076708_latest.zip
ENV ANDROID_HOME_PATH="/thirdparty/Android/Sdk"

RUN yes | /thirdparty/cmdline-tools/bin/sdkmanager --sdk_root="$ANDROID_HOME_PATH" --licenses
RUN /thirdparty/cmdline-tools/bin/sdkmanager --sdk_root="$ANDROID_HOME_PATH" --install "platforms;android-35"
RUN /thirdparty/cmdline-tools/bin/sdkmanager --sdk_root="$ANDROID_HOME_PATH" --install "build-tools;35.0.0"

RUN wget2 https://gstreamer.freedesktop.org/data/pkg/android/1.26.8/gstreamer-1.0-android-universal-1.26.8.tar.xz -O /thirdparty/gstreamer-1.0-android-universal-1.26.8.tar.xz
RUN mkdir -p /thirdparty/gstreamer-1.0-android-universal-1.26.8
RUN tar -xf /thirdparty/gstreamer-1.0-android-universal-1.26.8.tar.xz -C /thirdparty/gstreamer-1.0-android-universal-1.26.8
RUN rm /thirdparty/gstreamer-1.0-android-universal-1.26.8.tar.xz

RUN wget2 https://dl.google.com/android/repository/android-ndk-r25c-linux.zip -O /thirdparty/android-ndk-r25d-linux.zip
RUN unzip /thirdparty/android-ndk-r25d-linux.zip -d /thirdparty/
RUN rm /thirdparty/android-ndk-r25d-linux.zip

ENV THIRDPARTY_DEPS_PATH="/thirdparty/"
