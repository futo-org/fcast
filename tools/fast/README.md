# FCast's Automated Set of Tests

Run all tests:

```
$ cargo fast -vvvv run-all
```

## Usage

```
$ cargo fast --help
Usage: fast [OPTIONS] <COMMAND>

Commands:
  run-all
  help     Print this message or the help of the given subcommand(s)

Options:
  -H, --host <HOST>                          The host address of the receiver [default: 127.0.0.1]
  -p, --port <PORT>                          The port of the receiver [default: 46899]
  -s, --sample-media-dir <SAMPLE_MEDIA_DIR>  [default: ../fcast-sample-media]
  -v, --verbose...                           Increase logging verbosity
  -q, --quiet...                             Decrease logging verbosity
  -h, --help                                 Print help
```
