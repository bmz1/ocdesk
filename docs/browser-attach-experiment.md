# Experiment: ocdesk as opencode's desktop browser

_2026-11-08, opencode 2.2.6, real server + mock model. Spike:
[`dev/browser-attach-spike.mjs`](../dev/browser-attach-spike.mjs)._

## Question

opencode 2.2.6 ships `@opencode/plugin-browser`: the server defines the
agent's browser tools (`tools.browser.*` in code mode) and a `browser` skill,
and a **desktop client owns the browser** and serves those tools. opencode's
own Electron app does this with an embedded Chromium. Can ocdesk be that
desktop?

## Result: yes, end to end

Against a real `opencode2 serve` (2.2.6) with the mock model, the spike
attached to a session, and the agent's code-mode call ran through it:

```
attached (version 4)
command tabs.open {"url":"http://127.0.0.1:35303/"}   -> success
command snapshot  {"tabID":"tab_…"}                    -> success (refs e1…eN)
command click     {"tabID":"tab_…","ref":"e31"}        -> success
command tabs.focus                                     -> success
command screenshot                                     -> success
```

The agent's tool result had the structured output (title, snapshot text,
`files[0].path` = a server-side temp file) **and** the screenshot as an
inline image part, so the model sees the page. An earlier run clicked a
disabled button; the failure ("element is not enabled") went back to the
agent as a tool error. With no desktop attached the tools don't exist for
the agent (`Unknown tool 'browser.tabs.open'`); they appear on attach.

## Protocol (as used; source: `packages/plugin-browser`, `gui-plugins/src/browser/pane.ts`)

All over the existing API, same Basic auth, `?location[directory]=…`:

| Step | Call |
|---|---|
| Wait for `{type:"server.connected"}` on `GET /api/event` | the SSE stream ocdesk already reads |
| Attach | `POST /api/rpc/experimental.browser/attach {input:{sessionID, connectionID, version:4}}`; **stays pending** for the attachment's life; resolves `"closed"` or `"replaced"` |
| Ready | event `rpc.experimental.browser.control` `{type:"attached", connectionID, version:4}` |
| Tab inventory | `POST …/state {input:{sessionID, connectionID, state:{tabs:[Tab], focusedTabID}}}` |
| Work | event `control {type:"command", connectionID, requestID}` → `POST …/command` returns `{action:{type, …args}, files, …}` → run → `POST …/result {…, requestID, outcome}` |
| Outcome | `{type:"success", result:{value, files:[{id,name,mime,data(base64)}]}}` or `{type:"failure", code, message}` |
| Cancel | event `control {type:"cancel", requestID}` |

- `value` must match the operation's output schema (the server validates).
  `Tab = {id: "tab_<uuid>", url, title, loading, canGoBack, canGoForward,
  generation, loadError?}`.
- `tabs.open`/`tabs.close` must publish `state` (and get it acknowledged)
  **before** returning their result: the server resolves tab IDs only from
  `state`.
- Unimplemented operations return `{type:"failure", code:"unsupported"}`;
  opencode's own desktop does the same, so a partial implementation is fine.
- URL policy (server side): only HTTP(S) and `about:blank`; `file://` is
  refused ("Unsupported browser URL"). `localhost` is allowed.
- Attachment is **per opencode session**. A second attach for the same
  session replaces the first (seen: the old process got `"replaced"`); the
  replaced one must not re-attach on its own.
- Files: screenshots/downloads go back as bytes (≤ 5 MiB per transfer); the
  server writes them to its temp dir and attaches images for the model.
- `tunnel.*` methods relay TCP for remote servers (pages load through the
  server's network). Not needed when opencode runs on the same machine, which
  is ocdesk's setup.
- The rpc id is `experimental.browser`: expect changes between versions; it
  belongs in the pinned-version protocol checks.

## Gotchas seen

- A desktop whose browser died but whose process lives on keeps the
  attachment: the agent gets "Target … has been closed" instead of the tools
  disappearing. Losing the browser must end the attach request.
- Playwright error messages carry a call log with ANSI colours; send one line.

## What it would take in ocdesk (Rust)

| Part | Notes | Size |
|---|---|---|
| RPC client | attach (long-lived POST per session), state/command/result; route `rpc.experimental.browser.control` from the existing event pump | small |
| Chromium | find a system Chrome/Chromium/Edge (or bundled); launch with a separate profile, CDP over a pipe; launch lazily on the first command, not on attach | small–medium |
| Operations, phase 1 | `tabs.*`, `navigate`/`back`/`forward`/`reload`/`stop`, `snapshot` (CDP `Accessibility.getFullAXTree` → text with `eN` refs mapped to backend node ids), `click`/`fill`/`press`/`hover`/`scroll`/`select`/`check` (box model + `Input.*`), `wait`, `screenshot`, `console` | the bulk: ~1.5–2.5k lines |
| Operations, phase 2 | `evaluate`, `frames`, `dialog`, `network.*`, `files.*`, `fill_form`, `find` | medium |
| Leave unsupported | `trace.*`, `cpu.*`, `heap.*`, `lighthouse` | – |
| Panel | the Browser panel from the agent-browser branch, fed by CDP `Page.startScreencast` instead of agent-browser's stream; same frame pipeline, same Take control (CDP `Input.*`) | small (reuse) |

Design question to settle first: tools exist only while a session is
attached, so which sessions does ocdesk attach? Options: every session in
the open folders (one long-lived request each), only the selected/running
ones, or a per-session toggle. The browser itself can stay unlaunched until
the first command either way.

## Compared with the agent-browser branch

| | opencode plugin (this) | agent-browser |
|---|---|---|
| Install | nothing (Chrome on the machine) | npm package + `agent-browser install` |
| Agent knows how | built-in tools + built-in skill | skill file + the agent choosing the CLI |
| Results to the model | structured + screenshot images | shell output |
| ocdesk role | owns the browser and the tabs | observer of someone else's browser |
| ocdesk code | larger (CDP operations) | small (done) |

Recommendation: build on the plugin. Keep the agent-browser viewer only if
users ask for it; its panel UI carries over either way.
