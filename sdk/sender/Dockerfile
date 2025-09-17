# docker build --no-cache -t fcast/sender-sdk-dev:latest .
# docker run --rm -it -w /app/sdk/sender --entrypoint='sh' --network host -v ..:/app fcast/sender-sdk-dev:latest

FROM rust:1-alpine

RUN apk add --update alpine-sdk protobuf wget2 gcompat

RUN cargo install cargo-ndk
RUN cargo install cargo-hack --locked

RUN rustup target add aarch64-linux-android \
    armv7-linux-androideabi \
    x86_64-linux-android \
    i686-linux-android \
    x86_64-unknown-linux-gnu

RUN mkdir -p ndk
RUN wget2 https://dl.google.com/android/repository/android-ndk-r27d-linux.zip -O ndk/android-ndk-r27d-linux.zip
RUN unzip ndk/android-ndk-r27d-linux.zip -d ndk
ENV ANDROID_NDK_HOME="/ndk/android-ndk-r27d"
