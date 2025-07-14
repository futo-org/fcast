FROM ubuntu:24.04

# TZ
ARG DEBIAN_FRONTEND=noninteractive

ENV ANDROID_HOME=/Android/Sdk
ENV TZ=Etc/UTC

RUN apt update
RUN apt install -y zip wget tzdata

RUN apt install -y openjdk-21-jdk
RUN wget https://dl.google.com/android/repository/commandlinetools-linux-13114758_latest.zip
RUN unzip commandlinetools-linux-13114758_latest.zip
RUN mkdir -p $ANDROID_HOME
RUN mv /cmdline-tools $ANDROID_HOME

RUN yes | $ANDROID_HOME/cmdline-tools/bin/sdkmanager --sdk_root=$ANDROID_HOME --licenses
RUN $ANDROID_HOME/cmdline-tools/bin/sdkmanager --sdk_root=$ANDROID_HOME --install "platforms;android-36"
