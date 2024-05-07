# What is FCast?

FCast is a protocol designed for wireless streaming of audio and video content between devices. Unlike alternative protocols like Chromecast and AirPlay, FCast is an open source protocol that allows for custom receiver implementations, enabling third-party developers to create their own receiver devices or integrate the FCast protocol into their own apps.

# Building

Setup a C# development environment and type:

```
dotnet build
```

# Usage

Example usage of the fcast client.

```
# Play a mp4 video URL (1.0 playbackspeed explicit)
./fcast -h localhost play --mime_type video/mp4 --url http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4 -t 10 -s 1.0

# Play a mp4 video URL using WebSockets
./fcast -h localhost -c ws play --mime_type video/mp4 --url http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4 -t 10

# Play a local mp4
./fcast -h 192.168.1.62 play --mime_type video/mp4 -f /home/koen/Downloads/BigBuckBunny.mp4

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

# Set speed to double
./fcast -h localhost setspeed -s 2.0
```
