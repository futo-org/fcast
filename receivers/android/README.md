# FCast Android Receiver

The FCast Android Receiver primarily targets TV devices but is also supported on tablet and phone devices. Supported Android versions is from 7.0 and later.

# Debugging

Testing on a physical device tends to result in the best experience.

## TV Emulator

The Android TV emulator works for most testing during development, but there are emulator specific issues that needs to be noted:
* Some video sources when playing videos will randomly resize during video playback (seems to occur on non-standard source resolutions like 854x480, probably due to codec incompatibility).
* Sometimes playback will have 'green screen artifacts' when viewing a video or attempting to seek to a different position.

To connect a sender device to a receiver inside of the emulator, networking redirection must be enabled: https://developer.android.com/studio/run/emulator-networking#redirection

Commands:
* `telnet localhost 5554`
* `auth <YOUR_TOKEN>`
* `redir add tcp:46899:46899`
* `redir add tcp:46898:46898`

For your network interface, you must also use port forwarding to localhost on ports 46899 and/or 46898. Then you can connect the sender device to your host machine's IP address.

On linux you also may need to enable the kernel parameter to allow forwarding to localhost (interface is 'wlp11s0' for this example): `sudo sysctl -w net.ipv4.conf.wlp11s0.route_localnet=1`
