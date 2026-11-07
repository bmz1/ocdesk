# ocdesk

A native desktop client for the [opencode](https://opencode.ai) coding agent,
written in Rust with [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui).
No Electron, no webview, no JavaScript.

ocdesk is purely a client. It starts one `opencode serve` (opencode **2.x**)
as a child process and talks to it over its HTTP API (`/api/*`) and event
stream. All agent logic, tools, providers and permissions stay in opencode.

```
ocdesk (Rust/GPUI) ──HTTP──▶ opencode serve   (child process, 127.0.0.1, random password)
                   ◀──SSE───  GET /api/event   (one stream for every folder)
```

**Status:** M1–M3: folders and sessions, streaming markdown, selectable text,
collapsible reasoning, expandable tool cards, permission replies and latest-turn
file diffs. Send/stop, reconnect, crash recovery and dark theme included.
See *Roadmap* and [handoff.md](handoff.md).

![ocdesk markdown](docs/screenshots/m2-markdown.png)

![ocdesk tool details](docs/screenshots/m3-tool.png)

![ocdesk changes](docs/screenshots/m3-changes.png)

## Pinned versions

| What | Version | Why pinned |
|---|---|---|
| opencode | **2.2.6** (`@opencode/cli`) | ocdesk refuses anything below 2.x and warns if the version differs from the pin |
| gpui | `gpui-pre =0.3.11` = **zed@f816e1f** (via `gpui-kit =0.9.0`) | GPUI's API changes between snapshots |
| gpui-component | 0.9.0 (via `gpui-kit`) | must match the gpui snapshot |
| Rust | 1.97 (edition 2024) | |

Why `gpui-kit` and not a Zed git rev: see [docs/gpui-notes.md](docs/gpui-notes.md).
The API ocdesk relies on, with real samples: [docs/protocol-notes.md](docs/protocol-notes.md).

## Running

Requirements: **opencode 2.x** and a configured provider:

```sh
npm i -g @opencode/cli@2.2.6     # installs `opencode` and `opencode2`
```

(`opencode-ai` on npm and the `opencode.ai/install` script are the 1.x line;
ocdesk doesn't support 1.x. ocdesk looks for `opencode2`, then `opencode`;
`OCDESK_OPENCODE=/path/to/binary` overrides.)

```sh
cargo run --release              # or: cargo run --release -- ~/code/my-project
```

**Folders.** Sessions are grouped by folder, like projects in Codex. Add one
with **+** next to *Folders* (native folder picker; on Linux without an
xdg-desktop-portal it asks for a path instead), or start ocdesk with a folder
argument or `OCDESK_DIR`. On first run the launch directory is added. One
opencode sidecar serves all folders: each session is pinned to its
folder. Folders, collapsed state, theme, window size/position (and whether it
was maximized), sidebar width and visibility, the last open session and the
model/agent picked for new sessions are saved in
`~/.config/ocdesk/settings.json` (Linux) or
`~/Library/Application Support/ocdesk/settings.json` (macOS). The last open
session is reopened on start unless a folder is passed on the command line.
Drag the sidebar's right edge to resize it (double-click resets it).

**Keyboard.** `Mod` is Cmd on macOS and Ctrl elsewhere; `Mod+/` shows the list.

| Keys | Action |
|---|---|
| `Mod+N` | New session |
| `Mod+O` | Add folder… |
| `Mod+B` | Show/hide sidebar |
| `Alt+↑` / `Alt+↓` | Previous / next session (sidebar order, expanded folders) |
| `Mod+L` | Focus the prompt |
| `Mod+.` or `Esc` in the prompt | Stop the running turn |
| `Mod+Shift+D` | Toggle Changes / Conversation |
| `Mod+/` | Keyboard shortcuts |
| `Mod+Q` | Quit |

**Errors.** If opencode is missing, too old or crashes, the sidebar footer
shows why, with **Restart opencode** and **Copy details** (the error plus
opencode's last output lines); a banner above the composer explains why
prompts are paused. A conversation whose history can't be loaded shows the
error with a **Retry** button.

**Theme.** Dark by default. Set `"theme": "light"` or `"system"` in
`settings.json`, or `OCDESK_THEME=light|system`.

Logs go to the platform data dir, e.g. `~/.local/share/ocdesk/logs/ocdesk.log`
(Linux) or `~/Library/Application Support/ocdesk/logs/ocdesk.log` (macOS).
Set `OCDESK_LOG` (an `EnvFilter` string) to change verbosity; every SSE frame
is logged at `debug` under the `sse` target.

Linux build dependencies (Debian/Ubuntu):

```sh
sudo apt install libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libvulkan-dev \
  libx11-xcb-dev libxcb-xkb-dev libfontconfig-dev libssl-dev libasound2-dev libzstd-dev
```

## Developing

`dev/mock_llm.py` is a deterministic OpenAI-compatible streaming model, so
everything can be exercised offline and without API keys. It streams
reasoning, markdown, `bash` tool calls and `write` tool calls.

opencode reads providers from its config dir, so point `XDG_CONFIG_HOME` at
`dev/xdg`, which wires the mock in as provider `mock` (and makes it the
default model). This also keeps ocdesk's own dev settings in
`dev/xdg/ocdesk/` (git-ignored).

```sh
python3 dev/mock_llm.py &                       # listens on 127.0.0.1:4555
XDG_CONFIG_HOME=$PWD/dev/xdg cargo run -- /path/to/some/repo
```

Prompt keywords: `tool:bash` (calls the `shell` tool), `tool:edit`, `long`, `rich`;
anything else gets a short reply. To exercise permission prompts, add
`"permission": {"shell": "ask", "edit": "ask"}` to `dev/xdg/opencode/opencode.json`.

**Tools and permissions.** Click a tool card to expand its complete input and
output. Copy keeps the full text even when a large preview is shortened.
Pending permission requests appear above the composer, with a waiting badge in
the sidebar. Choose **Allow once**, **Always allow** (when the server supplies
save rules), or **Reject**. The exact rules being saved are shown beside the
actions. Failed replies stay visible and retryable. Reconnecting and selecting a
session re-fetch pending requests; ocdesk never replies automatically.

**Composer.** Type `@` to search the folder's files (and subagents such as
`@general`); ↑/↓ and Enter or Tab pick one, Esc closes the list. A picked file
becomes a chip and is attached to the prompt (opencode inlines its content).
The footer picks the **agent** (Build, Plan, …), the **model** (grouped by
provider) and, when the model has them, its **variant** (reasoning effort).
With nothing selected, the pick applies to the next new session and sticks.
↑ in an empty prompt recalls earlier prompts of the session; ↓ goes back.

**Changes.** The conversation header's **Changes** button shows the latest
turn's files, addition/deletion counts, and colored unified patches. Use
**Refresh** during an active turn; opened diffs refresh when a turn finishes.
**Conversation** returns to the message list.

**Offline UI demo (no opencode, no model).** `OCDESK_DEMO=<turns>` skips the
sidecar and seeds one session with synthetic history (`turns` × user +
assistant, default 20); prompts get a streamed markdown reply through the
same `Store::apply` path real events take. `OCDESK_DEMO=250` is the
500-message perf case. `OCDESK_DEMO_DELAY_MS` sets the per-word delay
(default 15), `OCDESK_DEMO_AUTOPLAY=<prompt>` sends one prompt at startup
(`long` in it gives the long answer).

```sh
OCDESK_DEMO=250 OCDESK_DEMO_AUTOPLAY="long answer" OCDESK_FRAME_LOG=1 cargo run
```

**Frame times.** `OCDESK_FRAME_LOG=1` logs, once a second while frames are
drawn, the CPU time per frame (root `render` to last `paint`: count, mean,
p95, max) and peak RSS under the `ocdesk::frames` target. Debug builds also
log it at `debug`.

Verification hooks (env vars):

- `OCDESK_PROBE_PROMPT=<text>`: send `<text>` in a new session once connected.
- `OCDESK_EXIT_ON_IDLE=1`: quit (via the normal quit path) when a turn ends.
- `OCDESK_VERIFY_ON_IDLE=1` (debug builds): when a turn ends, compare the streamed
  store against `GET /api/session/{id}/message` and log `verify: store matches REST` or `MISMATCH`.
- `OCDESK_DEBUG_PANIC=1` (debug builds): panic on connect, to test sidecar teardown.
- `OCDESK_FOLDER_DIALOG=1`: skip the native folder picker and ask for a path.

Unit tests (`cargo test`) replay real captured opencode 2.2.6 events
(`tests/fixtures/`) through the store and check the result equals the
server's REST projection; they also cover reconcile, the execution
lifecycle, folder targeting, version parsing and SSE framing.

`python3 dev/m3-protocol-test.py /path/to/opencode2` checks live permission
requests, restoring pending requests, once/always/reject replies, rejected tool
history and latest-turn edit diffs against real opencode 2.2.6. It creates a
temporary repository and isolated config/data, and starts its own mock model.

`dev/teardown-test.sh` (Linux, Xvfb) checks that the sidecar dies on SIGTERM,
SIGINT, SIGKILL, panic and normal quit.

Before every commit: `cargo fmt && cargo clippy --all-targets -- -D warnings`.

## Architecture

```
src/
  main.rs            bootstrap: logging, tokio runtime thread, signals, window
  sidecar.rs         spawn / health-check / kill `opencode serve`
  settings.rs        folders, theme, window/sidebar, last session, draft model/agent (JSON)
  api/mod.rs         thin HTTP client + SSE reader (the only place that does HTTP)
  api/model.rs       our view of sessions/messages/blocks (lenient JSON parsing)
  api/events.rs      server events → our normalized Event enum
  store.rs           Entity<Store>: folders, sessions, conversations, busy; folds events
  controller.rs      intents → API calls; folders, sidecar lifecycle, auto-restart, SSE pump
  ui/app_view.rs     root layout: sidebar | conversation; theme; window/sidebar persistence
  ui/keys.rs         app-wide shortcuts and the shortcuts sheet
  ui/sidebar.rs      folders with their sessions; new / rename / delete; connection
  ui/conversation.rs virtualized message list (MessageScroller), start screen
  ui/message.rs      one message: plain text, reasoning, tool cards
  ui/tool_card.rs    expandable tool input/output, errors, exit/truncation metadata
  ui/permission.rs   pending permissions with explicit allow/reject actions
  ui/changes.rs      read-only, color-coded latest-turn file patches
  ui/composer.rs     prompt box, Send ↔ Stop, agent/model/variant pickers, @ popup, history
  ui/mention.rs      @ query detection, tokens → prompt attachments, history (pure)
  ui/form_view.rs    question forms (form.*) above the composer
docs/
  protocol-notes.md              endpoints + real event samples (opencode 2.2.6)
  openapi-opencode-2.2.6.json    the server's own spec (GET /openapi.json)
  gpui-notes.md                  GPUI patterns and surprises
tests/fixtures/                  real 2.2.6 captures used by the unit tests
handoff.md                       state of the project for the next session
```

- **Threads:** GPUI owns the main thread. A dedicated multi-threaded Tokio
  runtime (`ocdesk-net-*` threads) runs reqwest and SSE. Results return over
  `futures` channels, awaited in `cx.spawn` tasks that update entities and
  call `cx.notify()`. The UI thread never blocks on I/O.
- **Sidecar lifecycle:** `opencode serve --port 0` in its own process group,
  with a random 32-character `OPENCODE_SERVER_PASSWORD` per launch (HTTP
  Basic, user `opencode`). The bound URL is parsed from stdout, then
  `/api/info` is polled. The whole process group is killed from
  `on_app_quit`, the panic hook, the SIGINT/SIGTERM handler and a drop guard.
  On Linux `PR_SET_PDEATHSIG` also covers `kill -9`. The last 200 stderr
  lines are kept for crash reports.
- **One sidecar for all folders:** sessions are created with
  `location.directory` and stay pinned to it; `GET /api/event` carries events
  for every location. A thin reqwest client (no SDK crate: the Rust
  `opencode-codes` crate targets the 1.x API).
- **Events:** our SSE reader emits explicit `Connected` / `Disconnected`,
  treats 30 s without a frame as a dead connection (the server heartbeats
  every 10-15 s) and reconnects with exponential backoff (250 ms → 5 s).
- **Busy state:** from `session.execution.started` /
  `succeeded|failed|interrupted`, re-seeded from `GET /api/session/active`
  after every reconnect.
- **State:** views never call HTTP. They call `Controller` intents
  (`send_prompt`, `abort`, `add_folder`, `rename_session`, …), which run on
  Tokio and fold results into `Store`. `Store` emits fine-grained events
  (`MessageInserted`, `MessageChanged`, …) so the virtualized list re-measures
  only the row that changed.
- **Reconcile:** on every (re)connect the session list, busy set and the
  selected conversation are re-fetched and merged into the store: the snapshot
  wins, except where locally streamed text strictly extends it (the server
  persists text slightly after streaming it) and except for messages newer
  than the snapshot. Other conversations are re-fetched when next selected.
- **Crash recovery:** if the sidecar dies it is restarted automatically
  (up to 3 times a minute), then the UI reconnects and reconciles. A Restart
  button covers the rest.

## Roadmap

- [x] **M0** Spike: pinned stack, window, sidecar lifecycle, prompt round-trip, protocol notes
- [x] **M1** Folders + sessions sidebar, plain-text streaming conversation, composer, reconnect + reconcile, opencode 2.x, dark theme
- [x] **M2** Incremental streaming markdown, code copy buttons, links, selectable text, collapsible reasoning, frame-time probe
- [x] **M3** Tool cards, permissions (`permission.asked`), diffs
- [x] **M4** Question forms, @mentions (files, subagents), model/variant/agent picker, prompt history
- [x] **M5** Keyboard, persistence of window/sidebar, error states
- [ ] **M6** Packaging (.app, AppImage), CI

## Design

The UI follows the Codex app's layout ideas: a quiet sidebar of folders with
their sessions, one centered conversation column (max 760 px), user messages
as soft right-aligned bubbles, assistant output as plain flowing text, tool
calls as expandable cards, and a rounded composer docked at the bottom (centered
on the start screen, with a folder chooser). Dark theme by default; colors
come from gpui-component's neutral theme.

## Known limitations

- Only verified on Linux (Xvfb + llvmpipe) so far. macOS is the primary
  target but hasn't been run yet.
- `@` mentions attach files only; directories from the file search are
  hidden. Recalled history entries come back as plain text (their mention
  tokens aren't restored).
- Changes shows the latest turn, not a full session or repository diff. Patch
  previews are bounded; Copy patch includes the full patch returned by opencode.
- On macOS a `kill -9` of ocdesk can orphan the sidecar: there is no
  `PDEATHSIG` equivalent. Every other exit path is covered.

