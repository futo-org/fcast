
# Terminal Sender

The terminal sender application is available from https://github.com/futo-org/fcast/tree/master/senders/terminal which can be used for playing media and controlling it on the receiver.

Currently you must build the application from source. You must setup a Rust development environment and build the application via `cargo build`.

??? info "Example Usage"

    ```
    # Play a mp4 video URL (1.0 playbackspeed explicit)
    ./fcast -H 127.0.0.1 play --mime-type video/mp4 --url http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4 -t 10 -s 1.0

    # Play a local mp4
    ./fcast -H 192.168.1.62 play --mime-type video/mp4 -f /home/koen/Downloads/BigBuckBunny.mp4

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

## MIME Types

The following MIME types are supported by the receiver applications.

=== "Streaming"

    * `application/vnd.apple.mpegurl`
    * `application/x-mpegURL`
    * `application/dash+xml`
    * `application/x-whep`

=== "Video"

    * `video/mp4`
    * `video/mpeg`
    * `video/ogg`
    * `video/webm`
    * `video/x-matroska`
    * `video/3gpp`
    * `video/3gpp2`

=== "Audio"

    * `audio/aac`
    * `audio/flac`
    * `audio/x-flac`
    * `audio/mpeg`
    * `audio/mp4`
    * `audio/ogg`
    * `audio/wav`
    * `audio/webm`
    * `audio/3gpp`
    * `audio/3gpp2`

=== "Image"

    * `image/apng`
    * `image/avif`
    * `image/bmp`
    * `image/gif`
    * `image/x-icon`
    * `image/jpeg`
    * `image/png`
    * `image/svg+xml`
    * `image/vnd.microsoft.icon`
    * `image/webp`
