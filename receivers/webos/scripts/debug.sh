#!/bin/bash

version=$(jq -r '.version' fcast-receiver/package.json)
scripts/build.sh
ares-install --device tv ./com.futo.fcast.receiver_${version}_all.ipk

ares-inspect --device tv -s com.futo.fcast.receiver.service &
sleep 6
ares-inspect --device tv --app com.futo.fcast.receiver
