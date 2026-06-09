FROM rust:1-trixie

RUN apt update && apt install -y build-essential cmake pkg-config automake autoconf libtool \
  libprotobuf-dev protobuf-compiler wget2 openjdk-21-jdk xz-utils curl git git-lfs
RUN git lfs install
