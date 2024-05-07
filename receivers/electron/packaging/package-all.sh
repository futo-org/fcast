#!/bin/sh
npm run build
sh package.sh linux-x64
sh package.sh linux-arm64
sh package.sh win32-x64
sh package-macos.sh darwin-x64
sh package-macos.sh darwin-arm64