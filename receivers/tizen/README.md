# FCast Tizen OS Receiver

The FCast Tizen OS Receiver is split into two separate projects `FCastReceiver` for frontend UI and `FCastReceiverService` for the background network service. The WebOS receiver is supported running on TV devices from Tizen OS 5.0 and later.

The TV receiver player is using the same simplified player used in the webOS receiver implementation. Future versions might support a more advanced player like the Electron player since Tizen OS video player is less limited compared to webOS.

# How to build

## Preparing for build

A docker file is provided to setup your build environment. From the root of the repository:

* **Build:**: `docker build -t fcast/receiver-tizen-dev:latest receivers/tizen`
**Run:**
```bash
docker run --rm -it -w /app/receivers/tizen --env-file=./receivers/tizen/.env \
    --entrypoint='bash' -p 26099:26099 -p 26101:26101 -v .:/app \
    fcast/receiver-tizen-dev:latest
```

You can then run the following commands to finish setup inside the docker container.

```
npm install
```

For signing the build artifact you must export the following environment variables or set them in your `.env` file:
```
CERT_PATH=/app/receivers/tizen/PATH_TO_CERTS
CERT_IDENTITY=YOUR_IDENTITY
CERT_AUTHOR_PASSWORD=YOUR_PASSWORD
CERT_DIST_PASSWORD=YOUR_PASSWORD
```

Directory structure should be as follows for storing certificates:
* Author certificates: `$CERT_PATH/author/$CERT_IDENTITY/author.p12`
* Distributor certificates: `$CERT_PATH/SamsungCertificate/$CERT_IDENTITY/distributor.p12`

## Build

To build the `.wgt` package run `scripts/build.sh`. Build artifact will be located at `REPO_ROOT/receivers/tizen/FCastReceiver/.buildResult/FCastReceiver.wgt`.
