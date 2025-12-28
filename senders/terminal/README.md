# What is FCast?

FCast is a protocol designed for wireless streaming of audio and video content between devices. Unlike alternative protocols like Chromecast and AirPlay, FCast is an open source protocol that allows for custom receiver implementations, enabling third-party developers to create their own receiver devices or integrate the FCast protocol into their own apps.

# Building

Setup a rust development environment and type:

```
cargo build
```

# Usage

Example usage of the fcast client.

```console
# Play a mp4 video URL (1.0 playbackspeed explicit)
./fcast -H 127.0.0.1 play --mime-type video/mp4 --url http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4 -t 10 -s 1.0

# Play a local mp4 (--file-server-port is optional)
./fcast -H 127.0.0.1 play --mime-type video/mp4 -f /home/koen/Downloads/BigBuckBunny.mp4 --file-server-port 8000

# Play a DASH URL
./fcast -H 127.0.0.1 play --mime-type application/dash+xml --url https://dash.akamaized.net/digitalprimates/fraunhofer/480p_video/heaac_2_0_with_video/Sintel/sintel_480p_heaac2_0.mpd

# Play local DASH content
cat dash.mpd | ./fcast -H localhost play --mime-type application/dash+xml

# Pause playing
./fcast -H 127.0.0.1 pause

# Resume playback
./fcast -H 127.0.0.1 resume

# Seek to time 100
./fcast -H 127.0.0.1 seek -t 100

# Listen for playback updates
./fcast -H 127.0.0.1 listen

# Stop playback
./fcast -H 127.0.0.1 stop

# Set volume to half
./fcast -H 127.0.0.1 set-volume -v 0.5

# Set speed to double
./fcast -H 127.0.0.1 set-speed -s 2.0

# Receive keyboard events
./fcast -H 127.0.0.1 -s KeyDown,KeyUp listen

# Show image playlist
cat image_playlist_example.json | ./fcast -H 127.0.0.1 play --mime-type application/json

# Play from video playlist
cat video_playlist_example.json | ./fcast -H 127.0.0.1 play --mime-type application/json
```
