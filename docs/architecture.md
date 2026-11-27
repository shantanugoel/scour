# Architecture

scour is one Rust binary. It runs as one local daemon (`scour serve`), and the agents on the machine use it.
The daemon asks DuckDuckGo by default and Bing only if a person turns it on. It uses an HTTP client that has
a browser TLS fingerprint. It listens only on `127.0.0.1:7268`. No flag, variable or file changes that address.
An agent searches through MCP over streamable HTTP at `/mcp`.
The planned browser tier will run headful Chrome on Xvfb (a virtual display) as a child process.
The daemon will drive Chrome through CDP (the Chrome DevTools Protocol) with `chromiumoxide`.

The main reason for this shape: blocks come mostly from the IP and from the fingerprint, not from the tool. Thus
most queries use the cheap tier. The release binary is 17657880 bytes. The idle daemon uses 11.0 MiB of memory.
The planned cascade will use the browser tier only for Google, and for an engine that the config or the query
sets to the browser method.

## Context

```mermaid
flowchart LR
  agent[AI agent] -- MCP over HTTP --> d[scour serve]
  cli[scour CLI] -- HTTP JSON --> d
  d -- HTTPS, IPv4, default --> ddg[DuckDuckGo]
  d -- HTTPS, IPv4, opt-in --> bing[Bing]
  d -. optional .-> p[Global proxy]
  p --> ddg
  p --> bing
  d -. planned, opt-in .-> eng[Brave, Mojeek]
  d -. CDP, planned .-> br[Headful Chrome on Xvfb]
  br -. planned .-> g[Google]
  br -. planned, browser method .-> bing
  br -. planned, browser method .-> ddg
  d -. challenge, planned .-> sol[Person through the local inspector link, or solver plugin]
```

## Parts

| Part | What it does | Technology | Status |
| --- | --- | --- | --- |
| Daemon | Serves the JSON API and MCP on `127.0.0.1:7268` only. One layer rejects every request with a foreign `Host` or `Origin`. | `tokio`, `axum` | built |
| MCP endpoint | Serves streamable HTTP at `/mcp`. It has one tool, `search(query)`. The answer carries the `GET /search` JSON as `structuredContent`, the `scour search` text as `content[0].text`, and `isError` when no engine is `ok` or `empty`. The tool description warns that titles and snippets are untrusted web text. Planned: an optional `engines` argument selects the engines and the method of each engine for one query (refer to "Data → Search result"). Also planned: the answer shows the open challenges. | `rmcp` (streamable HTTP, stateless JSON) | built |
| JSON API and CLI | `scour search "q"` uses `GET /search`. `scour status` uses `GET /status` to read engine health without sending an engine request. Planned: `GET /search` accepts the same `engines` list as MCP, and `scour search --engines duckduckgo,bing@browser "q"` sends it. | `axum`, `wreq` | built; `engines` planned |
| Parallel HTTP search | Starts one task per enabled engine in a `JoinSet`. Each task checks its engine's limits before it sends a request. Search reports every engine state in config order and merges the results round-robin. | `tokio` | built |
| Cascade | Selects the engines and the method of each engine for a query. The engines come from the query `engines` list, or else from all enabled engines. The method of each engine comes from the query, or else from the config. The cascade sends `http` engines through the HTTP tier and `browser` engines through the browser tier. It never changes the method of an engine after a block, because a browser on the same IP gets the same block. | Rust | planned |
| Engines | Each engine makes the request URL and parses the page. One `Engine` struct holds the name, base URL, profile, URL builder and page reader. Each engine is one file with the same shape. There is no `Engine` trait, because a function pointer is enough for the engines. DuckDuckGo is built as the only default engine. It uses `https://html.duckduckgo.com/html/?q=<q>&kl=us-en`, and it reads the target URL from the `uddg=` parameter of each `/l/` redirect link. Bing is built as an opt-in engine. Brave and Mojeek are planned as opt-in engines. Brave gave 7 good answers, then HTTP 429 on paced query 5, so it may need the browser tier. Mojeek has no saved result page. Google is planned as a browser-only engine. In the saved Google result page, each result link is an opaque `/goto?url=` token, and the token does not contain the target URL as text. The `<cite>` text is not a full URL either. How the Google engine gets the target URL is an open question in `docs/vision.md`. Planned: each engine has two methods, `http` and `browser`. The struct keeps one `request_url` and one `read`. `profile` becomes `http: Option<wreq_util::Profile>`, and `None` means the engine has no `http` method (Google). The `browser` method loads the same `request_url` in a tab and runs the same `read` on the HTML of the page after the load. `read` can also give `Page::Challenge(kind)` for a challenge page. `duckduckgo::read` gave `ok` on 3 of 3 pages from headful Chrome (#31). `bing::read` gives `Page::Challenge(cloudflare)` when the CSS selector `#turnstile-widget` matches. That selector was on 4 of 4 Turnstile pages and on 0 of more than 70 other Bing pages (#31). The current `bing::read` gives `empty` on a Turnstile page. A check for the `challenges.cloudflare.com` host is not a marker, because Bing result pages also load it. Not built: a step that opens the home page and types the query, as `openserp` does for Bing. Spike #31 did not test it. | `scraper` | built for DuckDuckGo and Bing; `browser` method planned |
| HTTP tier | Sends the DuckDuckGo and Bing requests with the Chrome TLS profile. Uses IPv4 addresses for hosts it resolves locally. Supports one optional proxy for all engine requests. | `wreq`, `wreq-util` | built for DuckDuckGo and Bing |
| Browser tier | Has one function, `fetch(url) -> Rendered`. `Rendered` holds the HTTP status of the main document, the HTML of the page after the load, and the tab. The engine `read` decides whether the page is a challenge, because each saved Bing result page also loads `challenges.cloudflare.com`. The tier serves every `browser` engine from one Chrome and one profile. Each query opens a new tab and closes it after `read`. A fixed number of query tabs can be open at a time; other browser queries wait for a free tab (`tokio::sync::Semaphore`). The wait counts as part of the send. A challenge tab stays open in the challenge queue and does not use a query slot. The tier starts headful Chrome on an Xvfb display on the first browser query, or it connects to a CDP URL that the user gives. Chrome starts with `--password-store=basic` and `--use-mock-keychain`. Without them, Chromium hangs on the system keyring before it loads a page. The spikes kept Chrome on IPv4 with a local SOCKS5 proxy and `--host-resolver-rules`. How Chrome stays on IPv4 without a user proxy is not tested. It stops the browser after an idle time, but not while a challenge is open. Each CDP call has a timeout. The browser uses the same proxy as the HTTP tier. Its profile is in a temporary directory that the daemon deletes when the browser stops. Its CDP port listens on `127.0.0.1` only. | `chromiumoxide` 0.9 | planned |
| Engine state | Each engine tracks its last state, block rest, pace tokens and rolling 24-hour request count. Planned: the `http` and `browser` methods of an engine share this one state. | Rust, in memory | built for DuckDuckGo and Bing |
| Challenge queue | Keeps each open CAPTCHA with an ID, a screenshot, a live link and an expiry time. One queue serves every `browser` engine: a Google reCAPTCHA and a Bing Turnstile are both challenges. An engine has at most one open challenge, because the challenge starts a rest and the resting engine opens no tab. The live link is the local Chrome inspector of the challenge tab. The agent passes it to a person. A solver plugin is planned for after the first release. The queue watches the tab for the done signal (refer to "Data"). It removes the challenge on the done signal or at expiry. | Rust; CDP target list | planned |
| Fixtures and canary | Saved DuckDuckGo and Bing pages test the parsers. Planned: `scour canary` sends one query to each engine and prints its health. Also planned: a saved Google result page and Google block page from the browser spike, and the headful Bing result and Turnstile pages from spike #31 (`fixture-bing-results.html`, `fixture-bing-turnstile.html`). The #31 saver removed inline `<script>` text, so the Turnstile marker test also uses an unchanged #14 result page (`bing-bing-headful-01.html`). | Rust tests | built for DuckDuckGo and Bing |
| Packaging | Has two Docker images. `scour` has only the binary and the cheap engines. `scour:browser` adds Chrome and Xvfb. The installed Chromium of the browser spike is 437 MB, so `scour:browser` is several hundred MB. The image size is not measured yet. On a Linux host without Docker, the user installs Chrome and Xvfb, and sets the path of Chrome. | Docker, static musl binary | planned |

## Data

- **Search result.** `GET /search?q=<text>` returns JSON. It has `query`, `results[]` with `url`, `title`,
  `content`, `engine` and `position`, and `engines[]` with `name`, `state`, `daily_count` and `daily_limit`.
  The merge takes one item per engine in config order for each rank. It keeps the first occurrence of each exact
  URL string, including that item's engine, and assigns consecutive positions starting at 1.
  The `engines[]` array stays in config order even when engine tasks finish out of order.
  `engines[].error` is present only when the state is `error`. `engines[].ready_at` is present for `blocked`
  and `limited`. It is an RFC 3339 UTC
  timestamp to the second. A missing `q`, or a query of only spaces, returns HTTP 400. `challenges[]` is
  planned. The field names are the same as in SearXNG, so the format is familiar. We do not promise SearXNG
  compatibility, and we copy no SearXNG code (AGPL). The API ignores other parameters, so a client that sends
  `format=json` still works.
  - Planned: `engines[].method` is `http` or `browser`. It is the method that the query used.
  - Planned: an optional `engines` list selects the engines for one query: `engines=duckduckgo,bing@browser` in
    `GET /search`, or `"engines": ["duckduckgo", "bing@browser"]` in MCP. Each item is `<name>` or
    `<name>@<method>`. A bare name uses the method from the config.
  - Planned: an item can name only an engine that the config enables. A person turns engines on; an agent
    does not.
  - Planned: a bad list stops the whole query with HTTP 400 (MCP `isError: true`), and no engine request goes
    out. The message names the item. Bad items are:
    1. An unknown name, an engine that is not enabled, a duplicate name, or an empty list.
    2. An unknown method, or `google@http`, because Google has no `http` method.
    3. An `@browser` item when the browser tier is off, for example
       `scour: bing@browser needs the browser; start scour serve with --browser`.
- **Engine state.** HTTP 403 and 429 give `blocked` for any engine. For other statuses, the page reader can
  identify a block from the body. DuckDuckGo challenge pages give `blocked` at any status. Any remaining
  non-200 response gives `error` with `HTTP <n>`. Network failures and timeouts also give `error`.
  On HTTP 200, parsed results give `ok`. A DuckDuckGo no-results marker gives `empty`. Unrecognized
  DuckDuckGo pages give `parse_failed`. Bing reports `empty` when its parser finds no results on HTTP 200.
  The CLI prints results and `ok` or `empty` summaries to stdout. It prints `blocked`, `limited`, `parse_failed`
  and `error` to stderr. If no engine returns `ok` or `empty`, stdout is empty and the CLI exits with code 1.
  Planned: the `browser` method uses the same states. A `Page::Challenge` gives `blocked`, starts the rest, and
  opens a challenge. The HTTP status comes from the main document of the tab.
- **Rest.** The first block starts a 10-minute rest by default. Each consecutive block doubles that duration, up to 4 hours.
  An `ok` response clears rest and resets the count. An `empty`, `parse_failed` or `error` response leaves both unchanged.
  A monotonic clock controls when the next request can go out. The daemon stores a wall-clock deadline for each rest window.
  Searches during that window skip the request and return `blocked` with the same `ready_at`.
  The CLI and MCP round the remaining time up to seconds.
  If the rounded time is 60 seconds or more, they show it in minutes rounded up.
- **Request limits.** Each engine HTTP request has a 10-second timeout and a limit of 10 redirects.
  The CLI waits up to 45 seconds for a response from the daemon.
  Planned: a browser query has its own timeout, which includes the wait for a free tab. A measurement in
  Milestone 2 sets the value. A timeout gives `error`.
- **Pace and daily limits.** Admission checks active rest first, then the 200-request rolling limit, then pace.
  Each engine starts with five tokens and refills one every 30 seconds, up to five.
  If the next token is more than 30 seconds away, the engine reports `limited` with the time it can next send.
  Otherwise, admission reserves the token and adds the request to the daily count. It releases the lock before any wait.
  If rest begins during the wait, the daemon cancels the send and refunds its daily reservation.
  Failed sends count. At the daily limit, `ready_at` is the oldest request's admission time plus 24 hours.
  Planned: `http` and `browser` requests of one engine use the same tokens and the same daily count.
- **Status.** `GET /status` returns `engines[]` in config order, with `name`, `last_state`, `daily_count` and `daily_limit`.
  `last_state` starts as `null` and tracks search outcomes, including requests skipped during rest or at a limit.
  `ready_at` appears only during active rest, at the daily limit, or when the next pace slot is more than 30 seconds away.
  `scour status` prints this snapshot, which sends no engine requests.
  Planned: the snapshot keeps one row for each engine and no method, because both methods share one state.
- **MCP answer.** `POST /mcp` carries one JSON-RPC message. A request needs `Content-Type: application/json` and
  `Accept: application/json, text/event-stream`. The server keeps no session: each request stands alone, and the
  answer is one JSON body, not an SSE stream. `tools/list` returns one tool, `search`, with the object argument
  `{query: string}`. The optional `engines: string[]` argument is planned (refer to "Search result").
  For a search answer, `tools/call` returns the `GET /search` JSON in `structuredContent` and
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
  `kind` is `recaptcha`, `cloudflare` or `unknown`. A Bing Turnstile challenge has the kind `cloudflare`.
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
  - For another engine, the done signal is a page in the tab that the engine `read` gives as results or `empty`.
    For Bing, the owner cleared Turnstile through the live link in 10 s (#31). The widget left the page, the tab
    URL stayed `/search?q=...`, and `bing::read` gave results. Bing set no Cloudflare cookie, so `expires_at` of a
    Bing challenge has no cookie to come from. Milestone 2 sets it.
  - After the done signal, the challenge leaves `challenges[]`, and the rest of that engine ends. The agent calls
    `search` again, and the browser session gives results.
- **Configuration.** One TOML file holds the enabled engines. The default path is `$XDG_CONFIG_HOME/scour/config.toml`,
  or `$HOME/.config/scour/config.toml`. The `engines` key is a list; it defaults to `["duckduckgo"]`, and a person
  can add `bing`. `scour serve --config <path>` reads another file. Invalid TOML stops the daemon with exit code 1.
  Planned: an item can set the default method, as in `["duckduckgo", "bing@browser", "google"]`. A bare name
  uses `http`, or `browser` for an engine with no `http` method (Google). The same exit code applies to
  `google@http`, to `bing` with `bing@browser`, and to a `browser` item when the browser tier is off.
  `[testing.base_urls]` applies to both methods.
  The same exit code applies to an unknown key, an unknown or duplicate engine name, an empty list, or a missing
  `--config` file. A TOML error shows a redacted parser message and the line number.
  The optional `proxy` key accepts an HTTP, SOCKS5, or SOCKS5H URL.
  An invalid proxy URL stops startup with exit code 1. The start line shows only the proxy scheme, host and port.
  Failed proxy connections return `error`. The engine client makes no direct fallback request.
  For local tests, `[testing].rest_seconds` sets a positive base duration.
  `[testing.base_urls]` maps enabled engine names to HTTP or HTTPS URLs. Unknown and disabled engine keys fail at startup.
  The listen address stays fixed at `127.0.0.1:7268`. A later milestone can add a listen flag
  for Docker. That change makes the localhost invariant weaker for that case, so the owner decides it then.
- **State.** The daemon writes no query and no result to disk. It prints one start line and no query. It installs
  no `tracing` subscriber, so a library log call cannot print a query. Engine limits and health live in memory.
  Each engine has its own mutex. The code releases it before any wait or network request.
  The browser profile is planned. The daemon will delete the profile when the browser stops.
  The `GOOGLE_ABUSE_EXEMPTION` cookie lives only in that profile, so a browser stop ends the solved session.
- **Between the parts.** The CLI talks to the daemon directly with HTTP JSON and bypasses all proxies.
  The daemon does not talk to a browser yet.
  That path is CDP over WebSocket on `127.0.0.1`, and it is planned. A solver plugin is planned for after the first release. It
  would use JSON on stdin and stdout, or a JSON POST.

## Choices

- **Rust.** `wreq-util` has current Chrome, Firefox and Safari TLS profiles in pure Rust. The release binary is
  17657880 bytes and the idle daemon uses 11.0 MiB of memory. A static musl build with `wreq` works: the engine
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
  for all engines, because headful Chrome on Xvfb used a peak PSS of 696 MiB and an idle PSS of 551 MiB (#32),
  and 3 to 11 s for each Google query in the spikes. PSS (proportional set size) counts each shared page one time.
  The 1.7 to 2.0 GiB of the browser spike was a sum of `VmRSS`, which counts shared pages one time for each process.
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
- **The method of each engine comes from the config or the query (planned).** The config sets the default method.
  An agent can set `@browser` or `@http` for one query. Google has only the `browser` method.
  - Reason: Bing over plain HTTP gave 0 of 238 matching results, and Google has no `http` method. A person can
    send an engine through the browser when its HTTP answers are bad. In headful Chrome, Bing was not stable:
    spike #31 got Turnstile and then off-topic results, and spike #32 got results on 6 of 6 queries.
  - The browser is not the default, because headful Chrome used a peak PSS of 696 MiB and 3 to 11 s for each
    query.
  - We did not switch an engine from HTTP to the browser after a block. A browser on the same IP gets the same
    block, and the switch would spend the engine's budget two times.
  - We did not switch on a mismatch either. Bing returns HTTP 200 with unrelated results, so scour cannot see
    the mismatch.
- **One state for both methods of an engine (planned).** Rest, pace and the daily count are for each engine,
  not for each engine and method. A Bing HTTP block and a Bing browser block come from the same IP and the same
  Bing limits.
  - We did not keep a state for each method. With it, `bing@browser` could go around a Bing rest. That is the
    same as the rejected automatic switch, done by hand.
- **`name@method` items (planned).** One flat list of strings is the same in the config, `GET /search` and MCP.
  The name `engines` and the comma list in `GET /search` follow SearXNG. `@` cannot occur in an engine name.
  - We did not use objects such as `{name, method}`, because they make the list longer for each agent call.
  - A bad item stops the whole query, and no engine request goes out. We did not report it as an engine `error`.
    That would mix a caller mistake with engine health and change `last_state`.
- **No OpenCode `browse` as the browser.** It drives the person's own signed-in browser through an extension. scour
  needs a separate browser with a temporary profile, so agent searches never use the person's Google or Bing
  account. It also needs an extension, a pairing step and a second tool. Its way to stay hidden is worth copying if
  Google detects `chromiumoxide`: it turns on no CDP domain that a page can detect, and runs its code in isolated
  worlds.
- **No Ardia as the browser.** Ardia is a Chromium fork with a Unix control socket. The goal was a browser with no
  CDP port. In spike #32, Ardia 0.2.49 turned off its control socket when its token server could not bind TCP. So
  the socket does not remove a port. Its search results and memory were not measured. Chrome stays, and a person can
  still connect another browser through a CDP URL.
- **IPv4 for local DNS.** IPv6 breaks DDG, Brave, Mojeek, Startpage and Qwant (SearXNG #6833).
  Direct engine connections and `socks5://` use the local IPv4-only resolver.
  With `http://` and `socks5h://`, the proxy resolves engine hostnames and chooses IPv4 or IPv6.
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
