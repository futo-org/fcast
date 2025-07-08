#!/bin/bash
version=$1

~/webOS_SDK/TV/Emulator/${version}/vm_remove.sh
rm -rf ~/webOS_SDK/TV/Emulator/${version}
unzip ~/webOS_SDK/TV/Emulator_tv_linux_${version}.zip -d ~/webOS_SDK/TV/
chmod 755 ~/webOS_SDK/TV/Emulator/${version}/*
~/webOS_SDK/TV/Emulator/${version}/vm_register.sh
