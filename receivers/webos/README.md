# FCast WebOS Receiver

The FCast WebOS Receiver is split into two projects: `fcast-receiver` for the frontend UI and `fcast-receiver-service` for the background network service.

## Status

This receiver was deprecated on 2026-04-28 (`92d4d3d0`) and removed from the repository on 2026-09-24 (`8d4fc4f6`). The last version before that, 2.0.1, speaks FCast protocol version 3.

Version 3.0.0 is that receiver restored, together with the `receivers/common` code it shares, and extended to protocol version 4.

It has been tested mostly with automated tests on a desktop machine. On a TV it has had one manual test: installed on an LG C2 and used to play two videos cast from Grayjay. See [Tests on a TV](#tests-on-a-tv).

## Protocol versions

The service runs on the Node.js version that ships with the TV, and version 4 needs TLS 1.3, which older TVs do not have.

| webOS TV | Node.js | Highest protocol version | mDNS TXT |
|----------|---------|--------------------------|----------|
| 5.0, 6.0 | 8.12 | 3 | as before |
| 22, 23, 26 | 12.21, 12.22, 20.12 | 4 | adds `v=4` and `fp` |

The Node.js versions are those LG documents for each webOS TV release; the version actually on a TV is written at the start of the service log and has not been checked on a device. What decides is the runtime, not the webOS number: the service enables version 4 when its Node.js offers TLS 1.3.

The version is chosen per connection. A version 2 or 3 sender keeps working on every TV, also while version 4 senders are connected: each sender gets the shared playback state in its own protocol version.

A single package serves all TVs. On webOS 5.0 and 6.0 the version 4 code is loaded but stays inactive.

## What version 4 supports

- Encrypted connection: plaintext `Version`, then TLS 1.3 on the same connection, with a self-signed certificate pinned by the `fp` fingerprint. The fingerprint is in the mDNS TXT record and in the QR code on the main screen (`fcast://r/...` connection URL).
- `Load` of one item or of a queue; `QueueInsert`, `QueueRemove`, `QueueItemSelected`; automatic advance when the queue asks for it; `StopPlayback`.
- Seek (`ProgressChanged`), `VolumeChanged`, `SpeedChanged`, pause and resume (`PlaybackStateChanged`), `SetProgressUpdateInterval`.
- External subtitles (`AddSubtitleSource`) in WebVTT or SubRip, and `ChangeTrack` for the tracks the player knows.
- Companion resources (`fcomp://`), also when one sender loads a resource that another sender provides.
- State sync between senders as the specification describes; request headers are never relayed to other senders, in any protocol version.
- `Error` messages with `packet_num`, and the 3 s / 6 s heartbeat.

### Limits

- **Tracks embedded in the media are not listed.** The player is an HTML5 video element. It reports one video track and one audio track (or the element's own `videoTracks` and `audioTracks` where the TV's browser has them) plus the external subtitles. Subtitle and audio tracks inside a Matroska, MP4, HLS or DASH stream cannot be selected through version 4. The caption button of the player still works for HLS and DASH.
- **Mirroring is not supported** and is not advertised. `StartMirroringSession` is answered with `Error(InvalidState)`.
- Bitmap subtitles are not supported.
- Disabling the video track hides the picture; disabling the audio track mutes it. Decoding goes on.
- A still image has no playback state in version 4, as in the reference receiver. Previous and next on the remote do not move through a version 4 queue of images; senders do.
- There is no gapless playback. At the end of an item the next one is loaded.
- An error in the middle of playback is reported to the sender that loaded the media and the state becomes `Buffering` until the player plays again. Only a failed load ends with `Idle`.
- Senders are not rate limited, and there is no limit on the number of connections.
- A version 3 playlist is still run by the frontend. Version 4 queue commands on it are answered with `Error(InvalidState)`.

### Changes for version 2 and 3 senders

Sharing one playback state between protocol versions, and running the upstream conformance suite against this receiver, changed some behaviour that 2.0.1 had:

- Request headers of a `Play` are no longer relayed to other senders in `PlayUpdate` or `Initial`. The sender that sent them still gets its own message back unchanged.
- At the end of the media the receiver reports `Paused` at the end position and keeps the media loaded. It used to report `Idle`. A resume or a seek plays it again.
- A pause sent while the media is still loading takes effect when it starts; `Playing` is not reported in between.
- An image on screen is reported as `Playing`.
- A `Play` with a URL that is not `http`, `https` or `fcomp` is refused with a `PlaybackError`.
- After a stop, `Idle` is reported by the service at once, and the player is paused before its page is replaced.
- `audio/mp3` is accepted as an audio type.

Defects of 2.0.1 fixed on the way:

- On Node.js 8.12 (webOS 5 and 6), events (`MediaItemStart` and the like) were never sent to the senders subscribed to them, and the proxy used for request headers threw on every request: both used JavaScript functions that Node.js 8 does not have.
- A play request arriving while the app was switching back to its main screen after a stop was lost.
- The mDNS advertisement was restarted every time the main screen loaded, and each restart left one more advertisement answering queries. In the test runs, after about ninety casts the service spent two thirds of its CPU time answering mDNS queries, in bursts of several seconds during which nothing else ran. It is now restarted only when the addresses of the TV change.
- Switching screens inside the app was reported to the service as the app being hidden, so the service relaunched the app on the next play request.
- Two service events of the same kind within one millisecond were delivered once.

### Receiver identity

The private key and the certificate are created on the TV the first time the service starts with version 4. They are not part of the package or of this repository.

They are stored in the `identity` folder inside the installed service. The fingerprint stays the same across restarts of the TV as long as that folder is kept. Senders that pinned the fingerprint must pair again when it changes:

- **Reinstalling or updating the app replaces the service folder**, so the receiver gets a new key and a new fingerprint.
- If the service folder cannot be written, the key is stored in the system temporary folder and is lost when the TV restarts.
- If neither can be written, the key lives in memory and changes every time the service starts.

The service log says which case applies: `v4 enabled fp=... identity=<folder or memory> persistent=<true|false>`.

## How to build

From `receivers/webos` (built here with Node.js 26 on the build machine):

```bash
(cd fcast-receiver && npm install && npm run build)
(cd fcast-receiver-service && npm install && npm run build)
npx --package @webos-tools/cli ares-package fcast-receiver/dist/ fcast-receiver-service/dist/ --no-minify
node scripts/check-ipk.js com.futo.fcast.receiver_3.0.0_all.ipk
```

`ares-package` works offline and writes `com.futo.fcast.receiver_3.0.0_all.ipk` to the current folder. `check-ipk.js` verifies that the package contains the fonts with their licence files, the icons and the background video, and that it contains no private key.

`scripts/build.sh` does the same with a globally installed `ares-package`.

### Generated code

The version 4 messages in `receivers/common/web/v4/generated` are generated with `flatc` 25.9.23 from `crates/fcast-protocol/flatbuffers/fcast.fbs`. `scripts/gen-v4-flatbuffers.sh` regenerates them.

## How to install on a TV

The TV must be in Developer Mode, with the Key Server on while the device is being set up.

```bash
npx --package @webos-tools/cli ares-setup-device   # once: name, IP address, port 9922, user prisoner
npx --package @webos-tools/cli ares-install --device <name> com.futo.fcast.receiver_3.0.0_all.ipk
npx --package @webos-tools/cli ares-launch --device <name> com.futo.fcast.receiver
```

The version from the LG store must be uninstalled first.

### Development container

The upstream Docker setup for building and debugging against a TV is unchanged. From the root of the repository:

```bash
source receivers/webos/.env && docker build --no-cache -t fcast/receiver-webos-dev:latest \
    --build-arg TV_IP=$TV_IP --build-arg PASSPHRASE=$PASSPHRASE receivers/webos/
```

The Key Server must be on during the build. `receivers/webos/.env` holds the values shown by the Developer Mode app:

```
TV_IP=YOUR_TV_IP_ADDRESS
PASSPHRASE=YOUR_TV_PASSPHRASE
```

The container may have to be rebuilt from time to time to keep the key up to date with the TV.

```bash
docker run --rm -it -w /app/receivers/webos --entrypoint='bash' --network host \
    -v .:/app fcast/receiver-webos-dev:latest
```

Host networking must be enabled in the Docker engine. Inside the container, `scripts/build.sh` builds the package and `scripts/debug.sh` runs it on the TV.

## Automated tests

All commands run from `receivers/webos/fcast-receiver-service` unless noted. The end-to-end tests use TCP port 46899, so only one of them can run at a time.

| Test | Command | What it covers |
|------|---------|----------------|
| Unit | `npm run test:unit` | framing, decoder limits, encoder, identity, discovery, connection URL, companion broker, playback coordinator |
| Service, version 3 | `node ../test/e2e/v3-baseline.js --service-dir <service> --sender <fcast> --fast <fast>` | the packaged service with version 2 and 3 senders |
| Service, version 4 | `npm run build:e2e`, then `node ../test/dist/e2e-v4.js --service-dir <service> --fast <fast> --sender-fp <fcast> --sender <fcast>` | handshake, TLS 1.3, pinning, heartbeat, protocol errors |
| Service and frontend | `node ../test/e2e/player-fast.js --service-dir <service> --frontend-dir ../fcast-receiver/dist --fast <fast> --media <dir>` | playback through the real frontend in a headless browser |

`<service>` is the service folder taken from the package (`node ../scripts/check-ipk.js <ipk> --extract <dir>`), or `dist/main` after a build. `<fast>` and `<fcast>` are the upstream conformance suite and terminal sender, built with `cargo build --release -p fast -p fcast` from the repository root.

The terminal sender learns the fingerprint only from mDNS. For `--sender-fp` it is built from a copy of the repository with `test/e2e-v4/terminal-sender-fp.patch` applied, which adds a test-only `--fp` option that supplies the fingerprint for a `--host` connection. `<dir>` holds the media FAST serves; `test/e2e/make-sample-media.sh <dir>` generates it with `ffmpeg` when the upstream `fcast-sample-media` repository is not available.

Set `FCAST_IDENTITY_DIR` to an empty folder so the tests do not write a key into the service folder.

The service and frontend test loads the compiled frontend, unchanged, in headless Chrome (or Edge, or the browser named by `FCAST_TEST_BROWSER`). Chrome is the first choice: with Edge, playback stopped advancing partway through a full run. Only the two webOS globals the frontend depends on are simulated (`PalmServiceBridge` and `PalmSystem`); they connect the page to the service running in the test process. The media plays in the browser's own video element.

### Results

Run on 2026-11-10 and 11 on Windows 11, against the service and the frontend extracted from `com.futo.fcast.receiver_3.0.0_all.ipk`, on the Node.js versions of the TVs (8.12.0 for webOS 5 and 6, 12.21.0 for webOS 22, 12.22.2 for webOS 23, 20.12.2 for webOS 26), using the official portable Node.js builds. Browser: headless Chrome.

| Test | 8.12.0 | 12.21.0 | 12.22.2 | 20.12.2 |
|------|--------|---------|---------|---------|
| Unit | not run (the test bundle needs Node.js 12) | 130/130 | 130/130 | 130/130 |
| Service, version 3 | 27/27 | 19/19 | 19/19 | 19/19 |
| Service, version 4 | no version 4 | 43/43 | 43/43 | 43/43 |
| Service and frontend | 33/33, 67 version 4 cases skipped | 100/100 | 100/100 | 100/100 |

- Service, version 3: on 8.12.0 the receiver announces version 3, so the upstream terminal sender is used as well (8 more steps). On the other versions it announces version 4 and the terminal sender is covered by the version 4 test.
- Service, version 4: includes 16 FAST cases (handshake, heartbeat, protocol errors), the upstream terminal sender with the right and with a wrong fingerprint, and mDNS discovery with the upstream sender.
- Service and frontend: one check of the connection URL drawn as QR code, plus 99 FAST cases (32 for versions 2 and 3, 67 for version 4).
- That the unit tests can fail was checked by hand: 19 defects seeded one at a time in the coordinator, the companion broker, the proxy and the encoder were each caught by at least one test.

The FAST media is synthetic (`make-sample-media.sh`), not the upstream sample files.

The table is the last run. Earlier runs of the service and frontend test failed in ways that depended on how long the run had lasted; each was traced before the run was repeated:

- The mDNS advertisements piling up (listed with the defects fixed) stalled the service for seconds at a time late in a run, which showed as TLS handshakes timing out and progress messages arriving late. Fixed in the receiver.
- With Edge as the browser, playback stopped advancing partway through a run. The same run completes with Chrome. Edge was not investigated further; putting a silent window to sleep is the suspected reason, not an established one.
- In one run on Node.js 12.22.2 the page stopped reacting partway through and every later case that needs the player failed. The cause was not established. The test bridge now keeps its subscription connection alive and reopens it if it drops; the runs since then completed, and none of them needed to reopen it.

### FAST cases that do not apply

`node ../test/e2e/player-fast.js --list` prints the cases that are run and those that are not, with the reason.

Of the 136 cases in the suite, 21 are not run:

- 3 are commented out of the case list upstream: `cast_video_set_volume_v2`, `cast_video_set_volume_v3`, `cast_video_set_volume_v4`.
- 17 need the audio, video or subtitle tracks embedded in a Matroska file, which an HTML5 video element does not expose (each was tried and stops waiting for a track list with two audio tracks or with embedded subtitles): `cast_gif_then_video_v4`, `subtitle_change_and_disable_v4`, `subtitle_change_keeps_playing_v4`, `subtitle_disable_while_paused_v4`, `subtitle_switch_while_paused_v4`, `video_disable_with_subs_v4`, `video_disable_while_paused_v4`, `video_disable_then_eos_v4`, `video_track_switch_v4`, `audio_track_switch_v4`, `audio_disable_enable_v4`, `track_change_rejections_v4`, `rapid_track_changes_v4`, `external_sub_keeps_embedded_selection_v4`, `external_sub_switch_embedded_external_v4`, `queue_autoplay_v4`, `gapless_queue_autoplay_v4`.
- 1 needs bitmap subtitles: `subtitle_bitmap_change_then_load_v4`.

What those cases cover apart from embedded tracks (track change checks, automatic advance of a queue) is covered by the unit tests of the playback coordinator, without a real player.

## Tests on a TV

Done once, on an LG C2 in Developer Mode with version 3.0.0:

- the package installs and the app opens on its main screen, with the QR code and the IP address;
- Grayjay for Android, which speaks protocol version 4, finds the receiver and connects to it;
- two videos cast from Grayjay played correctly: a YouTube video with subtitles and a 4K HDR video with subtitles.

Nothing else has been checked on a TV. The automated tests run the service on desktop Node.js and the frontend in a desktop browser; they say nothing about the TV's video decoder, its browser, the remote control or the app lifecycle.

Checklist for the rest, on a webOS 22 or later TV:

1. Install the package in Developer Mode and launch it. The main screen shows the QR code and the IP address.
2. In the service log, find `v4 enabled fp=...` and note `persistent=`.
3. From an upstream sender (Grayjay, or the terminal sender with `--name`), discover the receiver over mDNS and connect. The sender should report a version 4 connection.
4. Play an MP4 file and an HLS stream. Check picture, sound and the position reported on the sender.
5. Pause, resume, seek, change the volume and the speed from the sender. Check the TV follows and the sender shows the new state.
6. Load a queue of two videos with autoplay; check the second starts when the first ends. Use previous and next on the remote.
7. Add a WebVTT subtitle from the sender; check it is drawn and can be switched off.
8. Cast a local file from a sender that uses companion resources.
9. Connect a version 3 sender at the same time; check both follow the same playback.
10. Restart the TV and check the fingerprint in the service log has not changed.
11. Check the look of the main screen, the player controls and the toasts at 1080p.

Also not tested: webOS 5.0 and 6.0 TVs (version 3 only), and sources that need request headers on a TV.
