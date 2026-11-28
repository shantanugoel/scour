# scour

scour is a small, self-hosted metasearch tool that gives AI agents web search results without a block.

## Problem

A person who runs AI agents on their own machine needs web search for those agents. The search engines block
automated clients, and the tools that exist are heavy, slow or blocked.

One example: a developer runs some coding agents on one Linux machine. Each agent must search the web many times
each day. The developer uses forage today. Its image is approximately 3.85 GB. It uses 160 MiB of memory when idle
and 620 MiB at peak. One Google query takes 13 to 24 seconds. The developer tries SearXNG as an alternative.
Google gives results on 1 of 80 public SearXNG instances.

The worst failure is silent. An engine answers with HTTP 200 and no results, and the agent thinks that the web has
no answer.

## Vision

One local daemon gives search to all agents on the machine. It uses the cheapest method that works, and it tells
the truth when an engine blocks it.

A scenario:

1. The owner starts `scour serve` one time.
2. Each agent connects to the daemon through MCP over HTTP. A person or a script uses `scour search "q"`.
3. An agent sends a query. The daemon asks DuckDuckGo, and each other cheap engine that the owner turned on. It
   uses an HTTP client that looks like a browser.
4. An engine gives a block. The daemon finds the block and rests that engine. If another engine is on, the daemon
   uses it.
5. The agent gets the results, and the state of each engine. If each engine rests, the agent gets no results, the
   state `blocked`, and the time when the engine is ready again.
6. The owner starts the daemon with `--browser`. The daemon can then send an engine, such as DuckDuckGo, through
   headful Chrome on a virtual display. Google uses the browser. The owner or the agent can also send any
   enabled engine through Chrome.
7. Google shows a CAPTCHA. The daemon gives the calling agent a live link to that browser on `127.0.0.1`, and the
   agent passes it to a person.
8. The person opens the link, through an SSH tunnel from another computer, and solves the CAPTCHA. The agent
   searches again, and Google gives results.

## Users and devices

- AI agents, through MCP over HTTP.
- A person or a script, through the CLI (`scour search "q"`).
- Screens: none.

## Goals

The first useful version lets a person or an agent do these things:

- An agent can search the web through MCP over HTTP, and get results from DuckDuckGo, the default cheap HTTP
  engine. A person can turn on Bing, and Google with the browser. Brave and Mojeek are planned as opt-in engines.
  scour merges the results when two or more engines answer.
- An `engines` argument lets an agent select the engines for one query, and the method of each engine:
  HTTP or the browser (for example, `duckduckgo@browser`). The config sets the default method.
- A person can start one daemon (`scour serve`) that all agents on the machine share, and can search from the CLI.
  A person who runs scour on a server can send the engine requests through one proxy.
- An agent can see the state of each engine (`ok`, `empty`, `blocked`, `limited`, `parse_failed`, `error`).
  When an engine rests, the agent gets `blocked` and the time when the engine is ready again.
- The `--browser` flag starts the browser tier. A person can send any enabled engine through the browser.
  The tier uses headful Chrome on a virtual display (Xvfb). Chrome runs in a separate process.
  Google uses the browser. A CDP URL from a person is planned.
- The planned CAPTCHA handoff will let a person solve a CAPTCHA through a live link that the agent passes on.
  A person can open the link on the machine that runs scour, or from another computer through an SSH tunnel.

Planned for a later milestone: an agent can fetch one page as Markdown. A person can connect an external CAPTCHA
solver plugin.

## Non-goals

- We do not build a web UI, because the users are AI agents only.
- We do not run a public instance because each engine's `robots.txt` disallows the search path.
  The README states that scour is for personal use.
- We do not put a CAPTCHA solver in the core, and the calling agent does not solve CAPTCHAs. The reasons: Google v.
  SerpApi targets automated CAPTCHA circumvention, and most models refuse to solve CAPTCHAs. A solver is an
  external plugin (an external command or an HTTP endpoint) that the user adds on their own responsibility.
- We do not rotate proxies or keep a proxy pool, because rotation is the start of a scraping service. scour takes
  one optional proxy for all engines.
- We do not crawl sites and we do not extract data by a schema, because the scope is search first, then one page.
- We do not support Windows or macOS in the first release, because the appetite is 4 to 6 weeks. The first release
  is for Linux only.
- We do not start one process for each agent (MCP over stdio), because one shared daemon must own the engine state
  and the rate limits.
- We do not make Google a default engine, because Google needs JavaScript and a browser. Raw HTTP got 0 results in
  each test.
- We do not make Bing, Brave or Mojeek a default engine. In the cheap-engine spike, 0 of 238 plain HTTP Bing
  results matched the query. Brave gave HTTP 429 and Mojeek gave HTTP 403. A person can turn on Bing. Brave and
  Mojeek are planned.
- We do not promise that Bing works through the browser. A person can send Bing through the browser tier,
  but the results in headful Chrome were not stable. In the Bing-in-Chrome spike (#31), direct loads got Turnstile on
  3 of 4 tries. After a person solved it, 0 of 30 results matched the query. In the Ardia spike (#32), the same
  Chrome setup got Bing results on 6 of 6 queries, and 24 of 57 results matched all query words.
- We do not switch an engine from HTTP to the browser after a block, because a browser on the same IP gets the
  same block. The method of each engine comes only from the config or the query.
- We do not link a browser into the `scour` binary, because the only browser that got Google results is Chrome,
  and its installed size is 437 MB. A browser crash must also not stop the daemon. The browser runs as a child
  process.
- We do not use Obscura, Scrapling or headless Chrome for Google, because each got a reCAPTCHA on each query in
  the browser spike. Only headful Chrome on Xvfb got results. Its first session also failed, so a short IP block
  during those tests is possible.
- We do not put a browser in the default Docker image, because DuckDuckGo does not need one and Chrome adds
  hundreds of MB. The browser is in the `scour:browser` image.
- We do not build our own page that shows the browser to a person, because the local DevTools inspector of Chrome
  was sufficient in the handoff spike.

## Constraints

- **Platform and language:** Rust, because the owner selected it and a static binary is small. The spike program
  with `wreq` is a static musl binary of 8.2 MB, with a peak of 28.8 MiB of memory. Linux only for the first
  release. There are two Docker images: `scour` (the binary and the cheap engines) and `scour:browser` (the
  binary, Chrome and Xvfb). On bare Linux, the user installs Chrome or Chromium and Xvfb.
  scour runs `Xvfb` from `PATH`. For Chrome, it checks `CHROME`, then `chrome`, `chrome-browser`,
  `google-chrome-stable`, `chromium` and `chromium-browser` on `PATH`, in that order.
- **Cost:** The core uses no paid service. A CAPTCHA solver service is an external plugin that the user pays for.
  Tests make no paid model call.
- **Privacy and data:** scour runs on the machine of the user. It keeps no queries and no results on disk. It keeps
  the engine state in memory, and only the configuration file on disk.
- **Search budget:** DuckDuckGo is the only default engine, thus all agents on the machine share one budget.
  Measured: DuckDuckGo answered 60 of 60 queries in 60 minutes, at 1 request in 31 to 89 seconds. Not measured: a
  faster pace, a longer run, and the limit for one day. The research estimate is 50 to 100 queries each day from
  one IP. scour shows the daily count of each engine and stops an engine at its limit.
- **Licence:** Apache-2.0, the same as `wreq`. It includes a patent grant.
- **Appetite:** approximately 4 to 6 weeks of part-time agent work, to an open-source release after milestone 2.
  Then we stop and think again.

## Prior art

| Project | What it does well | Why it is not enough for us |
| --- | --- | --- |
| forage (Python, Scrapling, headful Chromium) | It gets Google results: 5 of 5 in our test. | The image is approximately 3.85 GB. It uses 160 MiB idle and 620 MiB at peak. A Google query takes 13 to 24 seconds. |
| SearXNG | It has many engines, and a JSON format that other tools know. | The engines block it. Google gives results on 1 of 80 public instances. It has a web UI that agents do not need. |
| OpenSERP (Go, go-rod, headless-shell) | It has REST, a CLI and MCP in a 211 MB image. | It uses a browser for each query, and it has a paid solver (2captcha) built in. |
| Websurfx (Rust, reqwest) | It is a Rust metasearch tool with a small footprint. | Its HTTP client does not look like a browser, and the engines block it. |
| Whoogle | It gave Google results with no JavaScript. | It is dead (July 2026), because Google now needs JavaScript. |
| FlareSolverr and Byparr | They get cookies for sites behind Cloudflare. | The method does not work for Google. Browser cookies in an HTTP client gave 0 results. |

## Open questions

| Question | How we answer it (a spike, the owner, a milestone) | What waits on it |
| --- | --- | --- |
| How does a person reach the live link when Chrome runs in Docker, with the CDP port on the loopback of the container? Which address does the daemon listen on in the container? Each answer can make the localhost invariant weaker. | The owner, in the plan of milestone 2 | The `scour:browser` image, and the handoff in Docker |
| Does the local inspector of Chrome work with a narrow `--remote-allow-origins` value? Each spike run used `*`. | Milestone 2 | The start flags of Chrome |
| Does a solved Chrome session stay unblocked with no search for hours? With one search each hour, a solved session gave Google results on 8 of 8 searches for 8 hours (#15). Google kept giving results after the `GOOGLE_ABUSE_EXEMPTION` cookie expired at 2h55m. An idle session is not tested. | Spike #36 | The idle time before the daemon stops the browser, and whether a keep-warm option is necessary |
| How long does a solved session that gets one search each hour last after 8 hours? No CAPTCHA came in the 8 hours of #15. The result is from one IP and one day. | No spike is planned. Milestone 2 can measure it in use. | How often Google needs a person |
| How often does a new headful Chrome session get a reCAPTCHA on its first Google query? 1 of 3 sessions did: 1 of 2 in the browser spike, and 0 of 1 in the Ardia spike (#32), which got results on 3 of 3 queries. | Milestone 2 | How often Google needs a person |
| How long does Chrome take from its start to a ready CDP port? | Milestone 2 | The idle time before the daemon stops the browser |
| How long does an unclicked Google `/sorry/` page stay solvable? Measured only: a clicked demo widget expired after approximately 2 minutes. | Milestone 2 | `expires_at` of a challenge |
| Does headful Chrome clear a Cloudflare challenge each time, when scour waits until the challenge ends? In the browser spike it cleared one challenge on its second load. The test was not fair to the other browsers. | The plan of milestone 3, with a spike as its first slice | The milestone that fetches a page as Markdown |
| Why do Bing results in headful Chrome change from one session to the next? Spike #31 got Turnstile and off-topic results; spike #32 got results on 6 of 6 queries. Do queries typed on the Bing home page work better? | No spike is planned. A later spike, if a person needs `bing@browser`. | Whether scour can promise `bing@browser` |
