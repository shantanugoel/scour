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

When several engines are enabled, the daemon queries them in parallel. It reports their states in config order.
The daemon takes one result from each engine in config order, then repeats for each rank.
It keeps the first occurrence of each exact URL and numbers the merged results from 1.

| State | Meaning |
| --- | --- |
| `ok` | The engine returned results. |
| `empty` | DuckDuckGo showed its no-results marker, or Bing returned HTTP 200 with no recognized results. |
| `blocked` | The engine is resting, returned HTTP 403 or 429, or DuckDuckGo showed a challenge page. |
| `limited` | The engine reached its daily limit, or its next pace slot is more than 30 seconds away. |
| `parse_failed` | DuckDuckGo returned HTTP 200 with neither recognized results nor a no-results marker. |
| `error` | The request failed, timed out, or returned another non-200 status without a recognized challenge. |

The CLI prints results and `ok` or `empty` summaries to stdout. It prints `blocked`, `limited`, `parse_failed` and `error` to stderr.
If no engine returns `ok` or `empty`, stdout is empty and the CLI exits with code 1.
Each engine summary includes the daily count and limit, for example `1/200 daily`.

After a block, the daemon skips requests to that engine for 10 minutes by default. Each consecutive block doubles the rest, up to 4 hours.
An `ok` response resets the backoff. An `empty`, `parse_failed` or `error` response leaves it unchanged.
The blocking search and searches during the same rest window report the same `ready_at` in RFC 3339 UTC.
Each engine starts with five pace tokens and gains one every 30 seconds, up to five.
A search can wait up to 30 seconds for a token. It checks for rest again before it sends the request.
Each engine allows 200 requests in a rolling 24-hour window. Failed requests count toward this limit.
While the engine rests or the search is limited, the daemon skips the request and adds nothing to the count.
The daemon keeps these counts in memory and resets them on restart.

Both `blocked` and `limited` search responses give a `ready_at` time.
The CLI and MCP text show the timestamp and the remaining time rounded up to seconds.
If the rounded time is 60 seconds or more, they show it in minutes rounded up.

3. Check engine status without sending a search.

```sh
./target/release/scour status
```

`GET /status` returns each engine's name, last state, daily count and daily limit.
Before the first search, the CLI shows `idle` and the JSON field `last_state` is `null`.
Status shows `ready_at` for active rest, a full daily window, or a pace wait longer than 30 seconds.
When a deadline appears, `scour status` also prints the remaining time.

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
code applies to an unknown key, an unknown or duplicate engine name, an empty list, or a missing `--config` file.
A TOML error shows a redacted parser message and the line number.

Set the optional `proxy` key to send all engine requests through one proxy.

```toml
proxy = "http://127.0.0.1:8080"
```

Use an `http://`, `socks5://`, or `socks5h://` URL with a host.
The default ports are 80 for HTTP and 1080 for SOCKS5.
For authentication, put the username and password before the host in the URL, with special characters percent-encoded.
The daemon prints only the proxy scheme, host and port at startup.
An invalid proxy URL stops startup with exit code 1.
If the proxy fails, the engine reports `error` and the daemon makes no direct request.

With `socks5://`, scour resolves engine hostnames locally and uses IPv4.
With `http://` and `socks5h://`, the proxy resolves engine hostnames and chooses IPv4 or IPv6.
The CLI connects to the local daemon directly, regardless of the config or environment proxy settings.

For local testing, set a positive base rest duration in seconds and point enabled engines at test servers.

```toml
[testing]
rest_seconds = 5

[testing.base_urls]
duckduckgo = "http://127.0.0.1:8080"
```

Each URL must use HTTP or HTTPS. A key for an unknown or disabled engine stops the daemon with exit code 1.
Each engine still uses its normal search path and query parameters.

## Check

Run:

```sh
scripts/check
```

The command checks the format, runs clippy with warnings as errors, and runs the tests. The tests use saved pages and loopback servers.

## Size and memory

The release binary size, in bytes:

```sh
stat -c %s target/release/scour
```

The measured binary size is 17657880 bytes.

The idle daemon memory, in kilobytes (`VmRSS`):

```sh
./target/release/scour serve --config /dev/null >/dev/null & pid=$!; sleep 1; awk '/^VmRSS:/ { print $2 }' /proc/$pid/status; kill $pid
```

The median of five starts is 11248 kB.

## Licence

scour is under the Apache License 2.0. The text is in `LICENSE`.
