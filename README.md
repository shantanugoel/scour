# scour

scour is a local search tool. One daemon asks Bing. You and your agents use that daemon.

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

The command prints 5 or more results. Each result has a title, a URL and a snippet. The URL is the address of the result site. The last line names the engine `bing` and the state `ok`.

## Check

Run:

```sh
scripts/check
```

The command checks the format, runs clippy with warnings as errors, and runs the tests. The tests make no request to Bing.

## Size and memory

The release binary size, in bytes:

```sh
stat -c %s target/release/scour
```

Measured value: 11688776 bytes.

The idle daemon memory, in kilobytes (`VmRSS`):

```sh
./target/release/scour serve >/dev/null & pid=$!; sleep 1; awk '/^VmRSS:/ { print $2 }' /proc/$pid/status; kill $pid
```

Measured value: 9368 kB.
