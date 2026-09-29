# CI image for the native android receiver. Everything the build needs is
# baked in, because none of it is optional: GStreamer is compiled from source
# by the gstreamer-src crate, so the meson/ninja toolchain is as load-bearing
# here as the NDK, and the slint android backend compiles java glue from a
# build script, so the JDK is too.
FROM rust:1-trixie

ARG DEBIAN_FRONTEND=noninteractive

ENV ANDROID_HOME=/opt/android/sdk \
    ANDROID_NDK_ROOT=/opt/android/android-ndk-r25c \
    JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64

# clang is not optional: .cargo/config.toml pins it as the HOST linker (bfd
# cannot resolve the static GStreamer link line in one pass), so without it
# every build script fails with "linker `clang` not found" long before
# anything android-shaped is reached.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential clang cmake meson ninja-build nasm flex bison gettext \
        pkg-config python3 git patch wget unzip xz-utils ccache \
        openjdk-21-jdk-headless ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Nightly, like the macOS receiver job. Arm targets only: x86_64 is emulator
# territory, which a cast target is not, and x86 has no linker in
# .cargo/config.toml. `xtask` refuses both.
RUN rustup default nightly \
    && rustup target add aarch64-linux-android armv7-linux-androideabi

# Same pinned fork the sender image uses.
RUN cargo install --locked cargo-ndk \
        --git https://github.com/bbqsrc/cargo-ndk \
        --rev 9672f442e3524139f3369720cd8d83c8f29b7303

# The other half of that pin: `--ld-path=wild`. Keep the version in step with
# the devshell's, so CI links the way the dev machines do.
RUN cargo install --locked wild-linker --version 0.9.0

# r25c: the version the android-linker-* wrappers and the packed-relocation
# and 16KB alignment flags in .cargo/config.toml are written against.
RUN wget -q https://dl.google.com/android/repository/android-ndk-r25c-linux.zip -O /tmp/ndk.zip \
    && mkdir -p /opt/android \
    && unzip -q /tmp/ndk.zip -d /opt/android \
    && rm /tmp/ndk.zip

# Platform 36 and build-tools 36: compileSdk in app/build.gradle and
# ANDROID_PLATFORM in xtask/src/android.rs both name that level, and the java
# glue compiles against the same android.jar gradle packages with.
RUN wget -q https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip -O /tmp/tools.zip \
    && mkdir -p /opt/android/cmdline \
    && unzip -q /tmp/tools.zip -d /opt/android/cmdline \
    && rm /tmp/tools.zip \
    && yes | /opt/android/cmdline/cmdline-tools/bin/sdkmanager --sdk_root=${ANDROID_HOME} --licenses > /dev/null \
    && /opt/android/cmdline/cmdline-tools/bin/sdkmanager --sdk_root=${ANDROID_HOME} \
        --install "platforms;android-36" "build-tools;36.0.0" "platform-tools"

# The base image puts cargo on PATH through ENV, which a login shell drops
# when /etc/profile rebuilds PATH. Runners and `docker exec -l` debugging
# sessions then find no cargo at all.
RUN printf 'export PATH=/usr/local/cargo/bin:$PATH\n' > /etc/profile.d/rust-cargo.sh
