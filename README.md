# ocdesk

A native desktop client for the [opencode](https://opencode.ai) coding agent,
written in Rust with [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui).
No Electron, no webview, no JavaScript.

ocdesk is purely a client. It starts one `opencode serve` as a child process
and talks to it over opencode's **v2 API** (`/api/*`, the API of opencode
2.0). All agent logic, tools, providers and permissions stay in opencode.

```
ocdesk (Rust/GPUI) ──HTTP──▶ opencode serve   (child process, 127.0.0.1, random password)
                   ◀──SSE───  GET /api/event   (one stream for every folder)
```

**Status:** milestone M1: folders with their sessions, live plain-text
streaming conversation, composer with send/stop, reconnect and sidecar crash
recovery, dark theme by default. Markdown, tool cards and permissions come in
M2/M3. See *Roadmap*.

![ocdesk conversation](docs/screenshots/m1-conversation.png)

## Pinned versions

| What | Version | Why pinned |
|---|---|---|
| opencode | **1.19.5**, v2 API (`/api/*`) | ocdesk warns at startup if `opencode --version` differs. The 2.0 `dev` build serves the same API. |
| gpui | `gpui-pre =0.3.11` = **zed@f816e1f** (via `gpui-kit =0.9.0`) | GPUI's API changes between snapshots |
| gpui-component | 0.9.0 (via `gpui-kit`) | must match the gpui snapshot |
| Rust | 1.97 (edition 2024) | |

Why `gpui-kit` and not a Zed git rev: see [docs/gpui-notes.md](docs/gpui-notes.md).
Why v2 and what's still v1: see [docs/protocol-notes.md](docs/protocol-notes.md).

## Running

Requirements: opencode on `PATH` (`npm i -g opencode-ai@1.19.5` or
`curl -fsSL https://opencode.ai/install | bash`) and a configured provider.

```sh
cargo run --release              # or: cargo run --release -- ~/code/my-project
```

**Folders.** Sessions are grouped by folder, like projects in Codex. Add one
with **+** next to *Folders* (native folder picker; on Linux without an
xdg-desktop-portal it asks for a path instead), or start ocdesk with a folder
argument or `OCDESK_DIR`. On first run the launch directory is added. One
opencode sidecar serves all folders: each v2 session is pinned to its
folder. Folders, collapsed state and theme are saved in
`~/.config/ocdesk/settings.json` (Linux) or
`~/Library/Application Support/ocdesk/settings.json` (macOS).

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

opencode's v2 API only reads providers from the config dir (it ignores
`OPENCODE_CONFIG_CONTENT`), so point `XDG_CONFIG_HOME` at `dev/xdg`, which
wires the mock in as provider `mock`. This also keeps ocdesk's own dev
settings in `dev/xdg/ocdesk/` (git-ignored).

```sh
python3 dev/mock_llm.py &                       # listens on 127.0.0.1:4555
XDG_CONFIG_HOME=$PWD/dev/xdg cargo run -- /path/to/some/repo
```

Prompt keywords: `tool:bash`, `tool:edit`, `long`; anything else gets a short
reply. `dev/mock-opencode.json` is the same provider with bash/edit
permissions set to `ask`, for permission work (M3).

Verification hooks (env vars):

- `OCDESK_PROBE_PROMPT=<text>`: send `<text>` in a new session once connected.
- `OCDESK_EXIT_ON_IDLE=1`: quit (via the normal quit path) when a turn ends.
- `OCDESK_VERIFY_ON_IDLE=1` (debug builds): when a turn ends, compare the streamed
  store against `GET /api/session/{id}/message` and log `verify: store matches REST` or `MISMATCH`.
- `OCDESK_DEBUG_PANIC=1` (debug builds): panic on connect, to test sidecar teardown.
- `OCDESK_FOLDER_DIALOG=1`: skip the native folder picker and ask for a path.

Unit tests (`cargo test`) replay real captured v2 events
(`tests/fixtures/`) through the store and check the result equals the
server's REST projection; they also cover reconcile, busy inference, folder
targeting and SSE framing.

`dev/teardown-test.sh` (Linux, Xvfb) checks that the sidecar dies on SIGTERM,
SIGINT, SIGKILL, panic and normal quit.

Before every commit: `cargo fmt && cargo clippy --all-targets -- -D warnings`.

## Architecture

```
src/
  main.rs            bootstrap: logging, tokio runtime thread, signals, window
  sidecar.rs         spawn / health-check / kill `opencode serve`
  settings.rs        folders, collapsed folders, theme (JSON in the config dir)
  api/mod.rs         thin v2 HTTP client + SSE reader (the only place that does HTTP)
  api/model.rs       our view of v2 sessions/messages/blocks (lenient JSON parsing)
  api/events.rs      v2 events → our normalized Event enum
  store.rs           Entity<Store>: folders, sessions, conversations, busy; folds events
  controller.rs      intents → API calls; folders, sidecar lifecycle, auto-restart, SSE pump
  ui/app_view.rs     root layout: sidebar | conversation; theme
  ui/sidebar.rs      folders with their sessions; new / rename / delete; connection
  ui/conversation.rs virtualized message list (MessageScroller), start screen
  ui/message.rs      one message: plain text, reasoning, one-line tool rows
  ui/composer.rs     prompt box, Enter / Shift+Enter, Send ↔ Stop
docs/
  protocol-notes.md              v2 (and v1) endpoints + real event samples
  openapi-opencode-1.19.5.json   the server's own spec (GET /doc)
  gpui-notes.md                  GPUI patterns and surprises
tests/fixtures/                  real v2 captures used by the unit tests
```

- **Threads:** GPUI owns the main thread. A dedicated multi-threaded Tokio
  runtime (`ocdesk-net-*` threads) runs reqwest and SSE. Results return over
  `futures` channels, awaited in `cx.spawn` tasks that update entities and
  call `cx.notify()`. The UI thread never blocks on I/O.
- **Sidecar lifecycle:** `opencode serve --port 0 --print-logs` in its own
  process group, with a random 32-character `OPENCODE_SERVER_PASSWORD` per
  launch (HTTP Basic, user `opencode`). The bound URL is parsed from stdout,
  then `/global/health` is polled. The whole process group is killed from
  `on_app_quit`, the panic hook, the SIGINT/SIGTERM handler and a drop guard.
  On Linux `PR_SET_PDEATHSIG` also covers `kill -9`. The last 200 stderr
  lines are kept for crash reports.
- **v2 API, one sidecar for all folders:** sessions are created with
  `location.directory` and stay pinned to it; `GET /api/event` carries events
  for every location. Rename, delete and legacy history use v1 routes until v2
  has them. We use a thin reqwest client: `opencode-codes` wraps the v1 routes.
- **Events:** our SSE reader emits explicit `Connected` / `Disconnected`,
  treats 30 s without a frame as a dead connection (the server heartbeats
  every 10-15 s) and reconnects with exponential backoff (250 ms → 5 s).
- **Busy state:** v2 has no idle event. Busy is inferred from step events and
  confirmed with `GET /api/session/active` where ambiguous (e.g. an interrupt
  during a tool), and re-seeded from it after every reconnect.
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
- [x] **M1** Folders + sessions sidebar, plain-text streaming conversation, composer, reconnect + reconcile, v2 API, dark theme
- [ ] **M2** Incremental streaming markdown, selectable text, collapsible reasoning (virtualized list already in)
- [ ] **M3** Tool cards, permissions (v2 `permission.v2.asked`), diffs
- [ ] **M4** @mentions, model/agent picker, history
- [ ] **M5** Keyboard, persistence of window/sidebar, error states
- [ ] **M6** Packaging (.app, AppImage), CI

## Design

The UI follows the Codex app's layout ideas: a quiet sidebar of folders with
their sessions, one centered conversation column (max 760 px), user messages
as soft right-aligned bubbles, assistant output as plain flowing text, tool
calls as one-line rows, and a rounded composer docked at the bottom (centered
on the start screen, with a folder chooser). Dark theme by default; colors
come from gpui-component's neutral theme.

## Known limitations

- Only verified on Linux (Xvfb + llvmpipe) so far. macOS is the primary
  target but hasn't been run yet.
- Sessions created by the 1.x TUI (v1 API) have no v2 history; ocdesk shows
  them read-only from the v1 history.
- v2 doesn't generate session titles yet; untitled sessions show as "New session".
- No permission UI yet (M3): with a config that asks for permission, a turn
  waits until stopped.
- On macOS a `kill -9` of ocdesk can orphan the sidecar: there is no
  `PDEATHSIG` equivalent. Every other exit path is covered.
