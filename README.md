# scour

scour is a local search tool. One daemon asks DuckDuckGo by default. You and your agents use that daemon.
A person can turn on Bing in a configuration file.

scour is for personal use, and nobody must run it as a public instance.

## Build

1. Install Rust.
2. Unset `LIBCLANG_PATH` if it points at another clang. An ESP clang makes the build look for a 32-bit header.
3. In this directory, run:

```sh
cargo build --release --locked
```

The program is `target/release/scour`.

## Run

The daemon listens on `http://127.0.0.1:7268`. No flag changes this address.

1. Start the daemon:

```sh
./target/release/scour serve
```

2. In a second terminal, search:

```sh
./target/release/scour search "rust async runtime"
```

The command prints 5 or more results. Each result has a title, a URL and a snippet. The URL is the address of the
result site. The last line names the engine `duckduckgo` and the state `ok`.

## Configure

By default, scour asks DuckDuckGo only. To turn on Bing, write this file:

```toml
# $XDG_CONFIG_HOME/scour/config.toml, or $HOME/.config/scour/config.toml
engines = ["duckduckgo", "bing"]
```

Or start the daemon with a file:

```sh
./target/release/scour serve --config /path/to/config.toml
```

The `engines` list accepts `duckduckgo` and `bing`. The daemon stops with exit code 1 for an unknown key, an unknown
or duplicate engine name, an empty list, or a missing `--config` file. A TOML error prints the parser message and
the line number, not the line text, so a secret in the file does not appear in the terminal.

## Check

Run:

```sh
scripts/check
```

The command checks the format, runs clippy with warnings as errors, and runs the tests. The tests make no request
to an engine.

## Size and memory

The release binary size, in bytes:

```sh
stat -c %s target/release/scour
```

Measured value: 11957968 bytes.

The idle daemon memory, in kilobytes (`VmRSS`):

```sh
./target/release/scour serve >/dev/null & pid=$!; sleep 1; awk '/^VmRSS:/ { print $2 }' /proc/$pid/status; kill $pid
```

Measured value: 9396 kB.

## Licence

scour is under the Apache License 2.0. The text is in `LICENSE`.
