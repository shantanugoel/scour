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

## Use MCP

The daemon also answers MCP over streamable HTTP at `http://127.0.0.1:7268/mcp`. It has one tool, `search`, with
one argument, `query`. `structuredContent` is the `GET /search` JSON, and `content[0].text` is the text of
`scour search`. A blank query, or an answer in which no engine returns `ok` or `empty`, sets `isError`.

To add the server to Claude Code:

```sh
claude mcp add --transport http scour http://127.0.0.1:7268/mcp
```

The tool description tells the agent that result titles and snippets are untrusted web text.

The daemon answers only a local request. The `Host` name must be `127.0.0.1` or `localhost`, with any port. The
`Origin` must be absent, or `http://127.0.0.1[:port]`, or `http://localhost[:port]`. Any other request gets HTTP
403 with `{"error":"the Host or Origin is not local"}`. This stops a web page from reaching the daemon through a
name that points at `127.0.0.1` (DNS rebinding).

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

The `engines` list accepts `duckduckgo` and `bing`. Invalid TOML stops the daemon with exit code 1. The same exit
code applies to an unknown key, an unknown or duplicate engine name, an empty list, or a missing `--config` file. A
TOML error prints the parser message and the line number, not the line text. Thus a secret in the file does not
appear in the terminal.

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

Measured value: 17361576 bytes.

The idle daemon memory, in kilobytes (`VmRSS`):

```sh
./target/release/scour serve >/dev/null & pid=$!; sleep 1; awk '/^VmRSS:/ { print $2 }' /proc/$pid/status; kill $pid
```

Measured value: 10576 kB.

## Licence

scour is under the Apache License 2.0. The text is in `LICENSE`.
