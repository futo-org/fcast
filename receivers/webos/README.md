# FCast WebOS Receiver

The FCast WebOS Receiver is split into two separate projects `fcast-receiver` for frontend UI and `fcast-receiver-service` for the background network service. The WebOS receiver is supported running on TV devices from WebOS TV 5.0 and later.

The TV receiver player is a simplified player compared to the Electron receiver due to functionality being redundant when using a TV remote control or due to platform limitations (https://gitlab.futo.org/videostreaming/fcast/-/issues/21).

# How to build

## Preparing for build

A docker file is provided to setup your build and debug environment for a local TV device. From the root of the repository:

### Build
```bash
source receivers/webos/.env && docker build --no-cache -t fcast/receiver-webos-dev:latest \
    --build-arg TV_IP=$TV_IP --build-arg PASSPHRASE=$PASSPHRASE receivers/webos/
```

Note that you must have the key server enabled during the build process.

To populate the container build arguments, you must export the following environment variables or set them in your `.env` file in `receivers/webos/.env`:
```
TV_IP=YOUR_TV_IP_ADDRESS
PASSPHRASE=YOUR_TV_PASSPHRASE
```

This information is found in the development app.

Note that you may have to periodically rebuild the container to keep key information up-to-date with the TV device.

### Run
```bash
docker run --rm -it -w /app/receivers/webos --entrypoint='bash' --network host \
    -v .:/app fcast/receiver-webos-dev:latest
```

Note that you must enable host networking support in your docker engine. Also, the production application
from the LG store must be uninstalled in order to run and debug the development version.

## Commands

* Build app: `scripts/build.sh`
* Debug app: `scripts/debug.sh`

Build artifact .ipk will be located in `receivers/webos`
