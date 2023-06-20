# What is FCast?

FCast is a protocol designed for wireless streaming of audio and video content between devices. Unlike alternative protocols like Chromecast and AirPlay, FCast is an open source protocol that allows for custom receiver implementations, enabling third-party developers to create their own receiver devices or integrate the FCast protocol into their own apps.

# Building

Setup a rust development environment and type:

```
cargo build
```

# Usage

Example usage of the fcast client.

```
# Play a mp4 video URL
./fcast -h localhost play --mime_type video/mp4 --url http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4 -t 10

# Play a DASH URL
./fcast -h localhost play --mime_type application/dash+xml --url https://dash.akamaized.net/digitalprimates/fraunhofer/480p_video/heaac_2_0_with_video/Sintel/sintel_480p_heaac2_0.mpd

# Play local DASH content
cat dash.mpd | ./fcast -h localhost play --mime_type application/dash+xml

# Pause playing
./fcast -h localhost pause

# Resume playback
./fcast -h localhost resume

# Seek to time 100
./fcast -h localhost seek -t 100

# Listen for playback updates
./fcast -h localhost listen

# Stop playback
./fcast -h localhost stop

# Set volume to half
./fcast -h localhost setvolume -v 0.5
```
