## Building

### Flatpak

Copy `cargo-sources.json` from https://gitlab.futo.org/-/snippets/4 (internal, TODO: provide it publicly upon release)
and place it in the root of the project (`fcast/`). Execute the following command from `fcast/senders/mirroring/desktop`:

```console
$ flatpak-builder --install ./flatpak-builddir --user org.fcast.sender.yml
```

`org.fcast.sender` should now be available on your system or execute the binary in `./flatpak-builddir/files/bin/fcast-sender`
(I believe this requires all the system dependencies to be available in the environment.)
