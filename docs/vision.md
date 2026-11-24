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
3. An agent sends a query. The daemon asks the cheap engines first, with an HTTP client that looks like a browser.
4. One engine gives a block. The daemon finds the block, rests that engine, and uses the other engines.
5. The agent gets the results, and the state of each engine.
6. The owner starts the daemon with `--browser`. Now the daemon can also ask Google through a browser.
7. Google shows a CAPTCHA. The daemon gives the calling agent a live browser link, and the agent passes it to a
   person.
8. The person solves the CAPTCHA in the live browser. The agent searches again, and Google gives results.

## Users and devices

- AI agents, through MCP over HTTP.
- A person or a script, through the CLI (`scour search "q"`).
- Screens: none.

## Goals

The first useful version lets a person or an agent do these things:

- An agent can search the web through MCP over HTTP, and get merged results from the cheap HTTP engines (Bing,
  DuckDuckGo, Brave, Mojeek and others).
- A person can start one daemon (`scour serve`) that all agents on the machine share, and can search from the CLI.
  A person who runs scour on a server can send the engine requests through one proxy.
- An agent can see the state of each engine (`ok`, `empty`, `blocked`, `parse_failed`). An empty answer and a
  block do not look the same.
- A person can turn on Google with `--browser`. The browser is a separate part that a person can replace.
- A person can solve a CAPTCHA through a live browser link that the agent passes on. A person can also connect
  an external solver plugin.

Planned for a later milestone: an agent can fetch one page as Markdown.

## Non-goals

- We do not build a web UI, because the users are AI agents only.
- We do not run a public hosted instance, because the `robots.txt` of each engine disallows the search path. scour
  is for personal use, and it carries a personal-use notice.
- We do not put a CAPTCHA solver in the core, and the calling agent does not solve CAPTCHAs. The reasons: Google v.
  SerpApi targets automated CAPTCHA circumvention, and most models refuse to solve CAPTCHAs. A solver is an
  external plugin (an external command or an HTTP endpoint) that the user adds on their own responsibility.
- We do not rotate proxies or keep a proxy pool, because rotation is the start of a scraping service. scour takes
  one optional proxy, for all engines or for one engine.
- We do not crawl sites and we do not extract data by a schema, because the scope is search first, then one page.
- We do not support Windows or macOS in the first release, because the appetite is 4 to 6 weeks. The first release
  is for Linux only.
- We do not start one process for each agent (MCP over stdio), because one shared daemon must own the engine state
  and the rate limits.
- We do not make Google a default engine, because Google needs JavaScript and a browser. Raw HTTP got 0 results in
  each test.
- We do not link a browser into the `scour` binary in the first release, because V8 makes the binary approximately
  70 MB and a V8 crash stops the daemon. The browser runs as a child process.
- We do not put a browser in the default Docker image, because the cheap engines do not need one. The browser is
  in the `scour:browser` image.

## Constraints

- **Platform and language:** Rust, because the owner selected it and a static binary is small (1.95 MB and 11.6 MiB
  of memory, measured for an `axum` server). Linux only for the first release. There are two Docker images:
  `scour` (the binary and the cheap engines) and `scour:browser` (the binary, a browser and Xvfb). On bare
  Linux, the user installs a browser and sets its path.
- **Cost:** The core uses no paid service. A CAPTCHA solver service is an external plugin that the user pays for.
  Tests make no paid model call.
- **Privacy and data:** scour runs on the machine of the user. It keeps no queries and no results on disk. It keeps
  the engine state in memory, and only the configuration file on disk.
- **Search budget:** an engine tolerates approximately 50 to 100 queries each day from one IP, at 1 request in 3 to
  10 seconds (research, not measured over days). Many busy agents on one IP can use this budget. scour shows the
  daily count of each engine, and rests an engine at its limit.
- **Licence:** Apache-2.0, the same as `wreq` and Obscura. It includes a patent grant.
- **Appetite:** approximately 4 to 6 weeks of part-time agent work, to an open-source release. Then we stop and
  think again.

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
| Can `chromiumoxide` drive the Obscura CDP server through one Google search (open, wait, read the DOM)? Does Obscura publish a library crate, and how large is it? | A spike ([#2](https://github.com/shantanugoel/scour/issues/2)) | The browser tier, and the selection of Obscura or Chrome as the default browser |
| From the IP of the owner, does Google give results to Obscura or to headless Chrome, or only to headful Chrome on Xvfb? | A spike ([#2](https://github.com/shantanugoel/scour/issues/2)) | The Google engine, and the contents and size of the `scour:browser` image |
| Which cheap engines keep giving results to `wreq` at a paced rate for some hours, with which TLS profile? Does `wreq` build into a static musl binary? | A spike ([#1](https://github.com/shantanugoel/scour/issues/1)) | The default engine list of milestone 1 |
| Does Obscura or headless Chrome pass a Cloudflare challenge page? | A spike ([#2](https://github.com/shantanugoel/scour/issues/2)) | The later milestone that fetches a page as Markdown |
| Can a person solve a CAPTCHA through a live link when the browser is Obscura, or does the handoff need Chrome? | A spike ([#3](https://github.com/shantanugoel/scour/issues/3)) | The handoff to a person |
| How does the agent learn that a person solved the CAPTCHA? How does the live link work safely when scour runs in Docker or on a server? | A spike ([#3](https://github.com/shantanugoel/scour/issues/3)) | The handoff to a person, and the interface for solver plugins |
