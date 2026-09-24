# C# SDK smoke test

Drives a real receiver through the packaged NuGet SDK: discovery, connect, load, seek, pause/resume,
volume, speed, stop, disconnect. Exits non-zero on the first step that times out.

1. Generate bindings: `cargo utask c-sharp build-c-sharp-library --release --out-dir ../FCastSenderSDKDotnet`
2. `dotnet pack -c Release` in `../FCastSenderSDKDotnet` (needs the native libs in `prebuilt/`)
3. Serve media with a server that supports Range requests (python's `http.server` does not, seeks then stall)
4. `dotnet run -p:SdkVersion=<packed version> -- <media url> <receiver mDNS name>`

Connect goes through discovery because v4 needs the TLS fingerprint from the TXT record.
Set `SMOKE_DEBUG=1` to print SDK debug logs.
