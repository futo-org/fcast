# RTARGET = aarch64-linux-android
RTARGET = x86_64-linux-android
OUT = app/src/main/jniLibs

.PHONY: default
default: check

.PHONY: check
check:
	cargo ndk --target $(RTARGET) check

.PHONY: build
build:
	cargo ndk --target $(RTARGET) -o $(OUT) build

.PHONY: build-release
build-release:
	cargo ndk --target $(RTARGET) -o $(OUT) build --release
