# Developing ocdesk

Everything you need to hack on ocdesk. Start with [CONTRIBUTING.md](../CONTRIBUTING.md)
for the checks a PR needs; the API ocdesk relies on is in
[protocol-notes.md](protocol-notes.md) and GPUI patterns in [gpui-notes.md](gpui-notes.md).

## Build dependencies

Linux (Debian/Ubuntu):

```sh
sudo apt install libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libvulkan-dev \
  libx11-xcb-dev libxcb-xkb-dev libfontconfig-dev libssl-dev libasound2-dev libzstd-dev
```

Rust comes from `rust-toolchain.toml` (rustup installs it automatically).

## Running against a mock model

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

**CI** (`.github/workflows/ci.yml`, every push to `master` and every PR):
Linux runs fmt, clippy, unit tests, shellcheck, then installs opencode 2.2.6
and runs `dev/m3-protocol-test.py` and `dev/teardown-test.sh` under Xvfb;
macOS runs clippy, unit tests and a build.

**Packaging** (`packaging/`, also what `.github/workflows/package.yml` runs):

```sh
packaging/linux/package.sh                    # AppImage + tarball in target/package/
SKIP_APPIMAGE=1 packaging/linux/package.sh    # tarball only
ARCHS="arm64 x86_64" packaging/macos/bundle.sh  # ocdesk.app, .dmg, .zip (on a Mac)
```

The AppImage is made with linuxdeploy (downloaded into `target/tools`).
macOS signing: set `MACOS_SIGN_IDENTITY` (a Developer ID Application identity)
and optionally `NOTARY_PROFILE` (from `xcrun notarytool store-credentials`).
In CI, add the repository secrets `MACOS_CERT_P12` (base64 .p12),
`MACOS_CERT_PASSWORD`, and for notarization `APPLE_ID`, `APPLE_TEAM_ID`,
`APPLE_APP_PASSWORD`; without them builds are ad-hoc signed. The app id is
`io.github.bmz1.ocdesk` (bundle id, Linux window class, desktop file).

**App icons.** `src/app_icon.rs` embeds `packaging/icon.png`: GPUI receives a
128 px window icon for X11, and AppKit sets the Dock image even for a bare macOS
executable. Finder uses the `.icns` in the packaged `.app`. For a Linux source
build, run `python3 dev/install-desktop.py target/debug/ocdesk` to register the
same desktop identity in `$XDG_DATA_HOME` (default `~/.local/share`); Wayland
resolves application icons through that desktop entry. Pass `--data-dir` to
verify registration in an isolated directory.

## Architecture

```
src/
  main.rs            bootstrap: logging, tokio runtime thread, signals, window
  shell_env.rs       login-shell PATH for apps started outside a terminal
  sidecar.rs         spawn / health-check / kill `opencode serve`
  settings.rs        app preferences, folders, theme, window/sidebar, last session (JSON)
  opencode_config.rs real JSONC discovery, syntax validation and conflict-checked saves
  api/mod.rs         thin HTTP client + SSE reader (the only place that does HTTP)
  api/model.rs       our view of sessions/messages/blocks (lenient JSON parsing)
  api/events.rs      server events → our normalized Event enum
  api/pty.rs         session terminals: persistent PTYs over a WebSocket
  store.rs           Entity<Store>: folders, sessions, conversations, busy; folds events
  controller.rs      intents → API calls; folders, sidecar lifecycle, auto-restart, SSE pump
  ui/app_view.rs     root layout: sidebar | conversation; theme; window/sidebar persistence
  ui/keys.rs         app-wide shortcuts, menu bar and the shortcuts sheet
  ui/kit.rs          shared components (status slot, monogram, context ring, toast…)
  ui/theme.rs        opencode theme + bundled fonts
  ui/palette.rs      command palette (Mod+K)
  ui/sidebar.rs      folders and sessions: status, pin, archive/undo, sections, Needs you
  ui/conversation.rs header, virtualized message list, find bar, right panel, status strip
  ui/message.rs      one message: prose, reasoning rows, turn footer, plan bar
  ui/tool_card.rs    work-log rows and tool cards, density
  ui/permission.rs   approvals above the prompt (keyboard: Enter / Esc / typed reply)
  ui/changes.rs      patch parsing and the diff view
  ui/review.rs       Changes tab: scopes, files, inline review comments
  ui/terminal.rs     Terminal tab (alacritty_terminal emulator)
  ui/syntax.rs       syntax colours for diffs, file reads and written files
  ui/todos.rs        todo list from todowrite: progress pill and checklists
  ui/images.rs       image thumbnails and the full-size viewer
  ui/composer.rs     prompt box, Send ↔ Stop, agent/model/variant pickers, @ popup, history
  ui/mention.rs      @ query detection, tokens → prompt attachments, history (pure)
  ui/form_view.rs    question forms (form.*) above the composer
docs/
  protocol-notes.md              endpoints + real event samples (opencode 2.2.6)
  openapi-opencode-2.2.6.json    the server's own spec (GET /openapi.json)
  gpui-notes.md                  GPUI patterns and surprises
tests/fixtures/                  real 2.2.6 captures used by the unit tests
packaging/                       icon, macOS bundle script + Info.plist, Linux AppImage/tarball
.github/workflows/               ci.yml (checks + e2e), package.yml (installers, draft release)
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
