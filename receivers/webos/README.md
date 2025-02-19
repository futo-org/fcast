# FCast WebOS Receiver

The FCast WebOS Receiver is split into two separate projects `fcast-receiver` for frontend UI and `fcast-receiver-service` for the background network service. The WebOS receiver is supported running on TV devices from WebOS TV 5.0 and later.

The TV receiver player is a simplified player compared to the Electron receiver due to functionality being redundant when using a TV remote control or due to platform limitations (https://gitlab.futo.org/videostreaming/fcast/-/issues/21).

# How to build

From `receivers/webos` directory:

## Prerequisites
```sh
npm install -g @webos-tools/cli
cd fcast-receiver
npm install
cd ../fcast-receiver-service
npm install
cd ../
```

## Build
```sh
cd fcast-receiver
npm run build
cd ../fcast-receiver-service
npm run build
cd ../
```

## Packaging
```sh
ares-package fcast-receiver/dist/ fcast-receiver-service/dist/ --no-minify
```

## Debugging
* Install: `ares-install --device tv ./com.futo.fcast.receiver_1.0.0_all.ipk`
* Web app debug: `ares-inspect --device tv --app com.futo.fcast.receiver -o`
* Service debug: `ares-inspect --device tv -s com.futo.fcast.receiver.service`
