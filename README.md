# scour

scour is a local search tool. One daemon asks DuckDuckGo by default. You and your agents use that daemon.
A person can turn on Bing in a configuration file, and Google with `--browser`.

scour is for personal use. Do not run scour as a public instance.

## Build

1. Install Rust 1.98 or later, `cmake`, a C/C++ toolchain and `libclang`.
2. Unset `LIBCLANG_PATH` if it points at another clang. An ESP clang makes the build look for a 32-bit header.
3. In this directory, run:

   ```sh
   cargo build --release --locked
   ```

The program is `target/release/scour`.

## Run

The daemon listens on `http://127.0.0.1:7268`. No flag changes this address.

1. Start the daemon.

   ```sh
   ./target/release/scour serve
   ```

2. Run a search in a second terminal.

   ```sh
   ./target/release/scour search "rust async runtime"
   ```

   To ask only some engines for one query, pass a comma list to `--engines`. Each item is an engine name, or a
   name with a method after `@`.

   ```sh
   ./target/release/scour search --engines duckduckgo "rust async runtime"
   ```

   An item can name only an engine that the config enables. The same list works in
   `GET /search?q=<text>&engines=duckduckgo` and in the MCP `engines` argument, which is a JSON array, as in
   `"engines": ["duckduckgo@http"]`.

3. Check engine status without sending a search.

   ```sh
   ./target/release/scour status
   ```

`scour search` prints the results that the engines return. With only DuckDuckGo enabled, its saved page gives 10 results.
Each result has a title, a URL and a snippet. A Google result also has an optional `domain`: the host from `<cite>`.
The URL of a Google web result is the Google redirect. The URL of a Google video card is the video address.

When several engines are enabled, the daemon queries them in parallel. A query with `--engines` asks only the
named engines. The daemon reports their states in config order. Each answer has `engines[]`, and each entry has
`name`, `method`, `state`, `daily_count` and `daily_limit`. `method` is `http` or `browser`.
The daemon takes one result from each engine in config order, then repeats for each rank.
It keeps the first occurrence of each exact URL and numbers the merged results from 1.

| State | Meaning |
| --- | --- |
| `ok` | The engine returned results. |
| `empty` | DuckDuckGo or Google showed its no-results marker, or Bing returned HTTP 200 with no recognized results. |
| `blocked` | The engine is resting, returned HTTP 403 or 429, DuckDuckGo showed a challenge page, or Google showed its `/sorry/` page. |
| `limited` | The engine reached its daily limit, or its next pace slot is more than 30 seconds or one refill interval away. |
| `parse_failed` | DuckDuckGo or Google returned HTTP 200 with neither recognized results nor a no-results marker. |
| `error` | The request failed, timed out, or returned another non-200 status without a recognized challenge. |

The CLI prints results and `ok` or `empty` summaries to stdout. It prints `blocked`, `limited`, `parse_failed` and `error` to stderr.
If no engine returns `ok` or `empty`, stdout is empty and the CLI exits with code 1.
Each summary line starts with `name@method`, for example `duckduckgo@http: ok (8 results) (1/100 daily)`.

A bad `engines` list stops the query with HTTP 400 (MCP `isError: true`), and no engine request goes out.
The message names the item. A bad list has an unknown name, an engine that is not enabled, a duplicate name,
an empty list, or an unknown method. An `@browser` item requires `scour serve --browser`.

After a block, the daemon skips requests to that engine for 10 minutes by default. Each consecutive block doubles the rest, up to 4 hours.
An `ok` response resets the backoff. An `empty`, `parse_failed` or `error` response leaves it unchanged.
The blocking search and searches during the same rest window report the same `ready_at` in RFC 3339 UTC.
By default, each engine starts with three pace tokens and gains one every 120 seconds, up to three.
A search can wait up to 30 seconds or one refill interval, whichever is shorter.
It checks for rest again before it sends the request.
By default, each engine allows 100 requests in a rolling 24-hour window. Failed requests count toward this limit.
While the engine rests or the search is limited, the daemon skips the request and adds nothing to the count.
The daemon keeps these counts in memory and resets them on restart.

After a paced run of 60 queries, DuckDuckGo blocked the test IP for more than 12 hours.
The research estimate was 50 to 100 queries per day from one IP. [Spike #14](https://github.com/shantanugoel/scour/issues/14) and [follow-up #35](https://github.com/shantanugoel/scour/issues/35).

Both `blocked` and `limited` search responses give a `ready_at` time.
The CLI and MCP text show the timestamp and the remaining time rounded up to seconds.
If the rounded time is 60 seconds or more, they show it in minutes rounded up.

`GET /status` returns each engine's name, last state, daily count and daily limit.
Before the first search, the CLI shows `idle` and the JSON field `last_state` is `null`.
Status shows `ready_at` for active rest, a full daily window, or a pace wait longer than 30 seconds or one refill interval.
When a deadline appears, `scour status` also prints the remaining time.

### Run with Chrome

Install Chrome or Chromium and put `Xvfb` on `PATH`. Set `CHROME` to the executable path to choose a specific Chrome.

```sh
./target/release/scour serve --browser
# In a second terminal:
./target/release/scour search --engines duckduckgo@browser "rust async runtime"
./target/release/scour search --engines google "rust async runtime"
```

Google only uses the browser. A bare `google` item is the browser method, and `google@http` is a bad item.
A Google web result URL is the absolute `/goto?url=` redirect. Spike #37 found no free source for the target URL,
so owner decision B keeps the redirect. A `/url?q=<target>` link uses the plain target, because it needs no extra
request. A video card uses its exact `data-curl` URL. Each result has `domain` when `<cite>` shows a host.
The `/sorry/` page gives `blocked`, and changed result markup gives `parse_failed`.

Chrome starts on the first browser query. Its DevTools port is fixed at `127.0.0.1:7269`.
A user-supplied Chrome DevTools URL is planned. HTTP and browser queries share the engine's limits and rest.
The daemon keeps the temporary profile under `/tmp`, because Chrome's Unix socket path has a length limit.
Stop the daemon with Ctrl-C or SIGTERM to close Chrome and Xvfb and delete the temporary profile.

## Docker

1. Build the image from this directory.

   ```sh
   docker build -t scour .
   ```

2. Start the container with the port published on host loopback.

   ```sh
   docker run -d --name scour -p 127.0.0.1:7268:7268 scour
   ```

The image contains the HTTP engines. Google needs Chrome and Xvfb, so this image cannot use it.
The image is about 138 MB on x86_64 and 159 MB on arm64.
Use `http://127.0.0.1:7268` on the host to reach the daemon.

On arm64, `docker build -t scour .` builds natively. On x86_64, enable arm64 emulation through `binfmt`
before using the following build command.

```sh
docker build --platform linux/arm64 -t scour:arm64 .
```

Copy the static aarch64 binary from `scour:arm64` into the current directory with `docker cp`.

```sh
container=$(docker create --platform linux/arm64 scour:arm64)
docker cp "$container:/usr/local/bin/scour" .
docker rm "$container"
```

Using `-p 7268:7268` exposes the daemon to the LAN. Other containers on the same Docker network can reach the
daemon even when the published port uses host loopback. If you run other containers, create a separate network
with `docker network create scour`. Add `--network scour` before the image name and leave unrelated containers
off that network.

Arguments after the image name go to `scour serve`. To use a config file, mount it read-only and pass its path
inside the container.

```sh
docker run -d --name scour -p 127.0.0.1:7268:7268 \
  -v "$PWD/config.toml:/etc/scour/config.toml:ro" scour --config /etc/scour/config.toml
```

## Use MCP

The daemon also answers MCP over streamable HTTP at `http://127.0.0.1:7268/mcp`. It has one tool, `search`, with
the required argument `query` and the optional `engines` list. `engines` takes the same items as `--engines`.
`structuredContent` is the `GET /search` JSON, and `content[0].text` is the text of
`scour search`. A blank query, a bad `engines` list, or an answer in which no engine returns `ok` or `empty` sets `isError`.

To add the server to Claude Code:

```sh
claude mcp add --transport http scour http://127.0.0.1:7268/mcp
```

The tool description tells the agent that result titles and snippets are untrusted web text.

The daemon requires exactly one `Host` header with `127.0.0.1` or `localhost`, with any valid port.
A request can have at most one `Origin` header. If present, it must be `http://127.0.0.1[:port]` or
`http://localhost[:port]`. The daemon rejects requests that fail these checks with HTTP 403 and
`{"error":"the Host or Origin is not local"}`. This stops a web page from reaching the daemon through a
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

The `engines` list accepts `duckduckgo`, `bing` and `google`. A bare name uses the engine default: `http` for
DuckDuckGo and Bing, and `browser` for Google.
With `scour serve --browser`, use `engines = ["duckduckgo@browser", "google"]` to send DuckDuckGo through Chrome
and to add Google.
An item that needs the browser stops startup with exit code 1 if the browser tier is off.
`google@http` is a bad item, because Google has no `http` method.
Invalid TOML stops the daemon with exit code 1. The same exit code applies to an unknown key, an unknown or
duplicate engine name, an unknown method, an empty list, or a missing `--config` file.
A TOML error shows a redacted parser message and the line number.

Set the optional `proxy` key to send HTTP engine requests through one proxy. Browser proxy support is planned.

```toml
proxy = "http://127.0.0.1:8080"
```

Use an `http://`, `socks5://`, or `socks5h://` URL with a host.
The URL has no path, query, or fragment. You can end it with a bare `/`.
The default ports are 80 for HTTP and 1080 for SOCKS5.
For authentication, put the username and password before the host in the URL, with special characters percent-encoded.
An unencoded `#`, `?`, or `/` in the password stops startup with exit code 1 and a message to percent-encode the password.
The daemon prints only the proxy scheme, host and port at startup.
An invalid proxy URL stops startup with exit code 1.
If the proxy fails, the engine reports `error` and the daemon makes no direct request.

With `socks5://`, scour resolves engine hostnames locally and uses IPv4.
With `http://` and `socks5h://`, the proxy resolves engine hostnames and chooses IPv4 or IPv6.
The CLI connects to the local daemon directly, regardless of the config or environment proxy settings.

Use `[limits]` to configure the pace and daily limit for every enabled engine. Each engine keeps its own counts.
This example shows the defaults. Each omitted key keeps its default.

```toml
[limits]
burst = 3
refill_seconds = 120
daily = 100
```

Use whole numbers from 1 to 1000 for `burst`, 1 to 86400 for `refill_seconds`, and 1 to 4294967295 for `daily`.
For an invalid value, the daemon exits with code 1 and names the key without showing its value.
An unknown key or an engine subtable such as `[limits.google]` also stops startup with exit code 1.

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

Install Chrome or Chromium, `openssl`, and either Xvfb or Docker. If `Xvfb` is absent from `PATH`, the first run
builds the `scour-check-xvfb` Docker image. If Docker is available, the check also tests that shim.
You can run the check while `scour serve --browser` has Chrome open.

```sh
scripts/check
```

The command checks the format and runs Clippy with warnings as errors.
It also runs the tests, which use saved pages, loopback servers, and headful Chrome.

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
