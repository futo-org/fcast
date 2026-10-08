# Unreleased

- Windows: "Share system audio" while mirroring, captured from the default output device via WASAPI loopback. Unlike the Linux null sink the audio keeps playing locally; if the device cannot be captured (held in exclusive mode, no output device) the cast continues with video only and says so. Optional "Mute this PC while casting" silences the local
  speakers for the duration of the cast (restored on stop, and at the next start after a crash).

# `0.0.1`

Init.
