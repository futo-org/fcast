# FCast WebOS Receiver

The FCast WebOS Receiver is split into two separate projects `fcast-receiver` for frontend UI and `fcast-receiver-service` for the background network service. The WebOS receiver is supported running on TV devices from WebOS TV 5.0 and later.

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
