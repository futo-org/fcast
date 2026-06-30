# FAST — FCast's Automated Set of Tests

`fast` drives a real FCast receiver through scripted protocol exchanges and
checks that it behaves correctly.

Run all tests against a receiver listening on the default address:

```
$ cargo fast run-all
```

Run a subset (each argument is matched as a substring of the test names):

```
$ cargo fast run cast_video cast_photo
```

Hammer the receiver by running the cases in a random order until one fails or
you interrupt it:

```
$ cargo fast stress
```

Add `-v`/`-vv`/`-vvv` for increasing log verbosity (the protocol traffic is
logged at debug level).

## Usage

```
$ cargo fast --help
Usage: fast [OPTIONS] <COMMAND>

Commands:
  run-all  Run all test cases
  run      Run specific test cases (matched as substrings of their names)
  stress   Run test cases in a random order forever, until interrupted or one fails
  help     Print this message or the help of the given subcommand(s)

Options:
  -H, --host <HOST>
          The host address of the receiver [default: 127.0.0.1]
  -p, --port <PORT>
          The port of the receiver [default: 46899]
  -s, --sample-media-dir <SAMPLE_MEDIA_DIR>
          [default: ../fcast-sample-media]
  -f, --fingerprint <FINGERPRINT>
          The receiver's certificate fingerprint for v4. When omitted, any server certificate is accepted during the v4 TLS upgrade
  -v, --verbose...
          Increase logging verbosity
  -q, --quiet...
          Decrease logging verbosity
  -h, --help
          Print help
```
