# Architecture

scour is one Rust binary. It runs as one local daemon (`scour serve`), and the agents on the machine use it.
Today the daemon asks DuckDuckGo by default and Bing only if a person turns it on. It uses an HTTP client that has
a browser TLS fingerprint. It listens only on `127.0.0.1:7268`. No flag, variable or file changes that address.
An agent searches through MCP over streamable HTTP at `/mcp`. A cascade and a browser for Google are planned too.
The browser is headful Chrome on Xvfb (a virtual display). It runs as a child process, and the daemon drives it
through CDP (the Chrome DevTools Protocol) with `chromiumoxide`.

The main reason for this shape: blocks come mostly from the IP and from the fingerprint, not from the tool. Thus
most queries use the cheap tier. The release binary is 17361576 bytes. The idle daemon uses 10.3 MiB of memory.
Only a Google query pays for a browser.

## Context

```mermaid
flowchart LR
  agent[AI agent] -- MCP over HTTP --> d[scour serve]
  cli[scour CLI] -- HTTP JSON --> d
  d -- HTTPS, IPv4, default --> ddg[DuckDuckGo]
  d -- HTTPS, IPv4, opt-in --> bing[Bing]
  d -. planned, opt-in .-> eng[Brave, Mojeek]
  d -. CDP, planned .-> br[Headful Chrome on Xvfb] --> g[Google]
  d -. challenge, planned .-> sol[Person through the local inspector link, or solver plugin]
```

## Parts

| Part | What it does | Technology | Status |
| --- | --- | --- | --- |
| Daemon | Serves the JSON API and MCP on `127.0.0.1:7268` only. One layer rejects every request with a foreign `Host` or `Origin`. | `tokio`, `axum` | built |
| MCP endpoint | Serves streamable HTTP at `/mcp`. It has one tool, `search(query)`. The answer carries the `GET /search` JSON as `structuredContent`, the `scour search` text as `content[0].text`, and `isError` when no engine is `ok` or `empty`. The tool description warns that titles and snippets are untrusted web text. A per-query `engines` argument and the open challenges are planned. | `rmcp` (streamable HTTP, stateless JSON) | built |
| JSON API and CLI | `scour search "q"` uses `GET /search`. `scour status` uses `GET /status` to read engine health without sending an engine request. | `axum`, `wreq` | built |
| Cascade | Selects the engines for a query and skips each engine that rests or is at its limit. It sends each HTTP engine through the HTTP tier, and each browser-only engine (Google) through the browser tier. It does not send a blocked HTTP engine to the browser, because a browser on a blocked IP does not help. It merges the results and removes duplicate URLs. | Rust | planned |
| Engines | Each engine makes the request URL and parses the page. One `Engine` struct holds the name, base URL, profile, URL builder and page reader. Each engine is one file with the same shape. There is no `Engine` trait, because a function pointer is enough for the engines. DuckDuckGo is built as the only default engine. It uses `https://html.duckduckgo.com/html/?q=<q>&kl=us-en`, and it reads the target URL from the `uddg=` parameter of each `/l/` redirect link. Bing is built as an opt-in engine. Brave and Mojeek are planned as opt-in engines. Brave gave 7 good answers, then HTTP 429 on paced query 5, so it may need the browser tier. Mojeek has no saved result page. Google is planned for the browser tier. In the saved Google result page, each result link is an opaque `/goto?url=` token, and the token does not contain the target URL as text. The `<cite>` text is not a full URL either. How the Google engine gets the target URL is an open question in `docs/vision.md`. | `scraper` | built for DuckDuckGo and Bing |
| HTTP tier | Sends the DuckDuckGo and Bing requests with the Chrome TLS profile. Uses IPv4 only. One optional proxy is planned. | `wreq`, `wreq-util` | built for DuckDuckGo and Bing |
| Browser tier | Has one function, `fetch(url) -> Page \| Challenge`. It starts headful Chrome on an Xvfb display on the first browser query, or it connects to a CDP URL that the user gives. Chrome starts with `--password-store=basic` and `--use-mock-keychain`. Without them, Chromium hangs on the system keyring before it loads a page. The spikes kept Chrome on IPv4 with a local SOCKS5 proxy and `--host-resolver-rules`. How Chrome stays on IPv4 without a user proxy is not tested. It stops the browser after an idle time, but not while a challenge is open. Each CDP call has a timeout. The browser uses the same proxy as the HTTP tier. Its profile is in a temporary directory that the daemon deletes when the browser stops. Its CDP port listens on `127.0.0.1` only. | `chromiumoxide` 0.9 | planned |
| Engine state | Each engine tracks its last state, block rest, pace tokens and rolling 24-hour request count. | Rust, in memory | built for DuckDuckGo and Bing |
| Challenge queue | Keeps each open CAPTCHA with an ID, a screenshot, a live link and an expiry time. The live link is the local Chrome inspector of the challenge tab. The agent passes it to a person. A solver plugin is planned for after the first release. The queue watches the tab for the done signal (refer to "Data"). It removes the challenge on the done signal or at expiry. | Rust; CDP target list | planned |
| Fixtures and canary | Saved DuckDuckGo and Bing pages test the parsers. Planned: `scour canary` sends one query to each engine and prints its health. Also planned: a saved Google result page and Google block page from the browser spike. | Rust tests | built for DuckDuckGo and Bing |
| Packaging | Has two Docker images. `scour` has only the binary and the cheap engines. `scour:browser` adds Chrome and Xvfb. The installed Chromium of the browser spike is 437 MB, so `scour:browser` is several hundred MB. The image size is not measured yet. On a Linux host without Docker, the user installs Chrome and Xvfb, and sets the path of Chrome. | Docker, static musl binary | planned |

## Data

- **Search result.** `GET /search?q=<text>` returns JSON. It has `query`, `results[]` with `url`, `title`,
  `content`, `engine` and `position`, and `engines[]` with `name`, `state`, `daily_count` and `daily_limit`.
  `engines[].error` is present only when the state is `error`. `engines[].ready_at` is present for `blocked`
  and `limited`. It is an RFC 3339 UTC
  timestamp to the second. A missing `q`, or a query of only spaces, returns HTTP 400. `challenges[]` is
  planned. The field names are the same as in SearXNG, so the format is familiar. We do not promise SearXNG
  compatibility, and we copy no SearXNG code (AGPL). The API ignores other parameters, so a client that sends
  `format=json` still works.
- **Engine state.** HTTP 403 and 429 give `blocked` for any engine. For other statuses, the page reader can
  identify a block from the body. DuckDuckGo challenge pages give `blocked` at any status. Any remaining
  non-200 response gives `error` with `HTTP <n>`. Network failures and timeouts also give `error`.
  On HTTP 200, parsed results give `ok`. A DuckDuckGo no-results marker gives `empty`. Unrecognized
  DuckDuckGo pages give `parse_failed`. Bing reports `empty` when its parser finds no results on HTTP 200.
  The CLI prints `blocked`, `limited`, `parse_failed` and `error` to stderr. It exits with code 1 if no engine returns
  `ok` or `empty`.
- **Rest.** The first block starts a 10-minute rest by default. Each consecutive block doubles that duration, up to 4 hours.
  An `ok` response clears rest and resets the count. An `empty`, `parse_failed` or `error` response leaves both unchanged.
  A monotonic clock controls when the next request can go out. The daemon stores a wall-clock deadline for each rest window.
  Searches during that window skip the request and return `blocked` with the same `ready_at`.
  The CLI and MCP round the remaining time up to seconds.
  If the rounded time is 60 seconds or more, they show it in minutes rounded up.
- **Pace and daily limits.** Admission checks active rest first, then the 200-request rolling limit, then pace.
  Each engine starts with five tokens and refills one every 30 seconds, up to five.
  If the next token is more than 30 seconds away, admission returns `limited` with that token's time.
  Otherwise, admission reserves the token and adds the request to the daily count. It releases the lock before any wait.
  If rest begins during the wait, the daemon cancels the send and refunds its daily reservation.
  Failed sends count. At the daily limit, `ready_at` is the oldest request's admission time plus 24 hours.
- **Status.** `GET /status` returns `engines[]` in config order, with `name`, `last_state`, `daily_count` and `daily_limit`.
  `last_state` starts as `null` and tracks search outcomes, including requests skipped during rest or at a limit.
  `ready_at` appears only during active rest, at the daily limit, or when the next pace slot is more than 30 seconds away.
  `scour status` prints this snapshot, which sends no engine requests.
- **MCP answer.** `POST /mcp` carries one JSON-RPC message. A request needs `Content-Type: application/json` and
  `Accept: application/json, text/event-stream`. The server keeps no session: each request stands alone, and the
  answer is one JSON body, not an SSE stream. `tools/list` returns one tool, `search`, with the object argument
  `{query: string}`. For a search answer, `tools/call` returns the `GET /search` JSON in `structuredContent` and
  the `scour search` text in `content[0].text`. A blank query sets `isError: true` and returns only the error
  message in `content[0].text`. A search answer sets `isError: true` when no engine is `ok` or `empty`.
  A blank query sends no engine request. The server reports `serverInfo` as `scour`.
- **Local-only guard.** One `Router::layer` wraps every route, including `/mcp`, `/search`, `/status` and unknown paths. A
  request requires one `Host` header with `127.0.0.1[:port]` or `localhost[:port]`. An optional `Origin` header
  must appear at most once and use `http://127.0.0.1[:port]` or `http://localhost[:port]`. Each optional port
  must contain only digits and have a value from 0 to 65535. Any other request gets HTTP 403 with
  `{"error":"the Host or Origin is not local"}`. The guard stops a web page from reaching the daemon through a
  name that points at `127.0.0.1` (DNS rebinding). `rmcp` also checks the `Host` by default; our layer runs first
  and answers in the same way on every route.
- **Challenge.** This shape is planned: `{id, engine, kind, page_url, live_url, expires_at}`. A screenshot is not in the shape, because only a solver plugin would use it.
  `kind` is `recaptcha`, `cloudflare` or `unknown`.
  - `live_url` is the local Chrome inspector of the challenge tab:
    `http://127.0.0.1:<port>/devtools/inspector.html?ws=127.0.0.1:<port>/devtools/page/<id>`. `<port>` is the CDP
    port, and `<id>` is the CDP target ID of the tab.
  - A person on another computer opens the link through `ssh -L <port>:127.0.0.1:<port> <host>`. The port number
    must be the same on both ends, because the link contains it.
  - An open reCAPTCHA widget expired approximately 2 minutes after a click, on a demo page. How long an unclicked
    Google `/sorry/` page stays solvable is not measured. Milestone 2 sets `expires_at` from a measurement.
  - The done signal for Google is the `GOOGLE_ABUSE_EXEMPTION` cookie, or a Google `/search` result page in the
    tab. The cookie expires approximately 3 hours after the solve. A URL that is no longer `/sorry/` is not enough,
    because the tab can go to a wrong page.
  - After the done signal, the challenge leaves `challenges[]`. The agent calls `search` again, and the browser
    session gives Google results.
- **Configuration.** One TOML file holds the enabled engines. The default path is `$XDG_CONFIG_HOME/scour/config.toml`,
  or `$HOME/.config/scour/config.toml`. The `engines` key is a list; it defaults to `["duckduckgo"]`, and a person
  can add `bing`. `scour serve --config <path>` reads another file. Invalid TOML stops the daemon with exit code 1.
  The same exit code applies to an unknown key, an unknown or duplicate engine name, an empty list, or a missing
  `--config` file. A TOML error shows a redacted parser message and the line number.
  For local tests, `[testing].rest_seconds` sets a positive base duration.
  `[testing.base_urls]` maps enabled engine names to HTTP or HTTPS URLs. Unknown and disabled engine keys fail at startup.
  The listen address stays fixed at `127.0.0.1:7268`. A later milestone can add a listen flag
  for Docker. That change makes the localhost invariant weaker for that case, so the owner decides it then.
- **State.** The daemon writes no query and no result to disk. It prints one start line and no query. It installs
  no `tracing` subscriber, so a library log call cannot print a query. Engine limits and health live in memory.
  Each engine has its own mutex. The code releases it before any wait or network request.
  The browser profile is planned. The daemon will delete the profile when the browser stops.
  The `GOOGLE_ABUSE_EXEMPTION` cookie lives only in that profile, so a browser stop ends the solved session.
- **Between the parts.** The CLI talks to the daemon with HTTP JSON. The daemon does not talk to a browser yet.
  That path is CDP over WebSocket on `127.0.0.1`, and it is planned. A solver plugin is planned for after the first release. It
  would use JSON on stdin and stdout, or a JSON POST.

## Choices

- **Rust.** `wreq-util` has current Chrome, Firefox and Safari TLS profiles in pure Rust. The release binary is
  17361576 bytes and the idle daemon uses 10.3 MiB of memory. A static musl build with `wreq` works: the engine
  spike binary was 8.2 MB, with a peak of 28.8 MiB. We did not use Go, because its fingerprint client
  (`tls-client`) replaces `net/http` and its HTTP/2 layer has no licence file; `chromedp` is better than the Rust
  CDP clients, but the browser is a separate process in any language. We did not use Zig, because it has no library
  that gives HTTP a browser TLS fingerprint, no CDP client and no HTML parser.
- **One shared daemon.** All agents share one engine state and one rate limit. Thus two agents do not burn the same
  IP. We did not use one stdio process for each agent, because each process would count the rate limit alone.
- **DuckDuckGo as the only default engine.** In a paced 60-minute run from the home IP, cookie-free DuckDuckGo
  answered 60 of 60 queries, with no home-page visit and no cookie jar. 58% of its results matched all query words.
  Bing answered each query, but in the 24 plain HTTP variants only 0 of 238 results matched all query words.
  Headful Chrome matched 58% on 2 of 3 queries, so the client causes the mismatch. Brave gave HTTP 429 with a
  CAPTCHA, Mojeek gave HTTP 403, and Startpage, Qwant, Yahoo and Ecosia gave no results. We did not keep Bing as a
  default, because a result that does not match the query is worse than no result: the agent cannot see the
  difference. When the only enabled engine reports a block, the answer has no results. Pace and daily limits
  reduce the chance of a block.
- **The CDP client in the daemon.** `chromiumoxide` drives Chrome through all steps of a Google search, so
  `headless_chrome` is not necessary. A CDP endpoint is the replaceable boundary: a user can give the CDP URL of
  another browser. The handoff link comes from the same CDP server.
  - We did not use a separate `scour-browser` worker with a JSON protocol on stdin, because that protocol has only
    one user. A move to a worker later stays cheap, because all browser code is behind `fetch`.
  - We did not use the `agent-browser` CLI for each step. It starts one process for each step, and the user must
    install a second tool. It also gives less control of the fingerprint and of the live session that the handoff
    needs. A user can still connect scour to an `agent-browser` session through its CDP URL.
- **Headful Chrome on Xvfb.** It was the only browser that got parsed Google results from the home IP (2 of 2 in the
  last session; the first session got 2 reCAPTCHA pages). Obscura 0.2.7, Scrapling and headless Chrome each got
  reCAPTCHA pages (0 of 2). The first headful session also failed, so a short IP block during those rows is possible. We did not use Obscura, because it got no Google results, it has no library crate on
  crates.io, and its stealth build is 164 MB. We did not use headless Chrome, because Google blocked it. Headful
  Chrome also cleared a Cloudflare challenge on its second load. That test was not fair to the other browsers.
- **The browser as a child process, not a library.** A browser crash does not stop the daemon, and the binary stays
  small. Chrome is not available as a library. We did not link a V8 browser such as Obscura, because V8 makes the
  binary approximately 70 MB, a V8 crash stops the daemon, and Obscura got no Google results.
- **Cheap HTTP engines by default, Google opt-in.** DuckDuckGo answered each paced query in a median time of
  approximately 0.7 s. Google gave no results to raw HTTP or to replayed browser cookies. We did not use the browser
  for all engines, because headful Chrome on Xvfb used 1.7 to 2.0 GiB of peak memory and 3 to 11 s for each
  Google query in the spikes.
- **Avoid and hand off. No automatic solver in the core.** The daemon rests the engine, uses other engines and slows
  down. Then it gives the CAPTCHA to a person through a live link. An external solver plugin is planned for after the
  first release. We did not put
  a solver in the core, and we did not give the agent a `solve` tool, for four reasons:
  1. Google v. SerpApi (a DMCA 1201 case) is about automated circumvention of CAPTCHAs.
  2. A solver needs a paid account.
  3. A solver hides the blocks that the health states must show.
  4. Most models refuse to solve CAPTCHAs, and a reCAPTCHA grid needs many rounds of clicks, not one text answer.
- **The local Chrome inspector as the live link.** The owner solved a Google reCAPTCHA through it in 26 s, from
  another computer through an SSH tunnel. The same session then gave Google results on 5 of 5 searches. The link
  gives full control of the browser (`Runtime.evaluate`, `Network.getCookies`, `Page.navigate`), so it stays on
  `127.0.0.1`. A person on another computer uses an SSH tunnel.
  - We did not use the appspot `devtoolsFrontendUrl` that Chrome gives, because its socket closes and the page does
    not show.
  - We did not build a screencast page in the daemon. The inspector is sufficient, and a screencast page that
    connects to the CDP socket gives the same full control.
- **The done signal from the cookie or the result page.** The `GOOGLE_ABUSE_EXEMPTION` cookie and a Google `/search`
  result page show that Google accepted the solve. We did not use "the URL left `/sorry/`" alone, because a person
  can move the tab to a wrong page, and that would look like a solve.
- **One proxy, no rotation.** A user on a server with a flagged datacenter IP can send requests through a home or
  residential proxy. We did not build a rotating pool, because it needs its own health checks and it is the start
  of a scraping service.
- **Health from the response status and body.** A DuckDuckGo challenge can arrive with HTTP 200 or 202.
  The explicit no-results marker lets the DuckDuckGo reader report `parse_failed` when result markup changes.
- **A browser only for browser-only engines.** The planned cascade will rest a blocked HTTP engine.
  A browser on the same IP gets the same block.
- **IPv4 only.** IPv6 breaks DDG, Brave, Mojeek, Startpage and Qwant (SearXNG #6833).
- **State in memory.** This is the smallest store that works for one daemon. We did not use SQLite or a file. Add
  one if restarts reset the daily limits too often.
- **The CLI uses the JSON API, not an MCP client.** `curl` and SearXNG clients can use the same endpoint. The CLI
  needs no MCP session. We did not use an MCP client in the CLI, because it adds a session for one call.
- **A stateless MCP server with no session ID.** The daemon holds no per-client MCP state, and every request is one
  JSON body with one answer. The CLI, a plain JSON-RPC script, and an agent use the same daemon, and a restart
  loses nothing that a client must renew. We did not use `legacy_session_mode`, because a session ID adds a
  handshake and a session table for a tool that holds no conversation state. If a client needs sessions, turn that
  mode on and record why.
- **Two Docker images.** Users who do not need Google do not download a browser. We did not use one image, because
  Chrome makes it several hundred MB larger: the installed Chromium is 437 MB. `scour:browser` has one browser,
  not two.

## Unknowns

The technical questions that are open are in "Open questions" in `docs/vision.md`, each with its spike.
