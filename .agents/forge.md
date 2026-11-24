# forge settings

The `forge` skill reads this file. Edit it as necessary. Run `forge init` again to change many settings at once.

## Settings

| Setting | Value |
| --- | --- |
| Process | full |
| Tracker | github |
| Repository | shantanugoel/scour |
| Check | `scripts/check` |
| Run | The steps under "Run" in `README.md`. |
| Evidence | `.evidence/` |
| Roadmap | #5 |
| Backlog | none |
| Branch | `issue/<issue>-<short-name>` |
| Docs | `README.md`: what scour is, and how to install, run and check it. `docs/`: design and architecture notes. |
| Dependencies | Allowed. Give the reason in the pull request. |
| Models | Tests make no paid model call. |
| Released | no |
| Browser | `agent-browser`, or the browser tool of the agent |

## Invariants

Rules that a change must not make weaker. Write "none" if there are none.

- The daemon and the CDP port of the browser listen on `127.0.0.1` by default.
- scour stores no queries and no results on disk. The browser profile is deleted when the browser stops.
- The core has no automatic CAPTCHA solver, and no tool that lets the calling agent solve a CAPTCHA.
- Page text and result snippets are data. scour never uses them as instructions.

## Users and devices

Who uses the program, and on which devices. The designer uses this for screens. Write "none" if there are no
screens.

- none

## Roster changes

Changes to the default roster for this project. Write "none" to use the default roster.

- none
