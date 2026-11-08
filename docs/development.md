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
(`long` in it gives the long answer). `OCDESK_DEMO_APPROVAL=1` adds a pending
command approval and `OCDESK_DEMO_ERROR=1` ends the history with a failed step.

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

See [State machines and transition ownership](state-machines.md) for the lifecycle
inventory, async ownership contracts and regression scenarios.

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

## Async state ownership

`domain/history.rs` owns the pure availability/request transition: Unloaded →
Loading → Ready, or Ready → Refreshing → Ready. Failure/cancellation releases the
request while preserving an existing baseline. The Store owns a private ticket
allocator across all sessions: eviction/deletion cannot reuse a ticket that an
old HTTP response still carries. Completion requires the active ticket, so stale,
foreign-session and duplicate responses cannot install history or clear errors.
The store's completion adapter executes the existing reconciliation algorithm and
emits the same fine-grained `StoreEvent`s; it does not clone the whole store.

At stream disconnect and sidecar restart, the controller cancels pending history
state and clears its bounded event buffers. Reconnect still re-fetches selected
history and invalidates other cached projections for selection-time reload.
Cached rows remain available during refresh. Overflow cancels the snapshot and
requires another read rather than silently dropping events.

`domain/requests.rs` separates `ServerGeneration` from `ConnectionEpoch`.
`RequestScope::Connection` rejects a response after either a stream boundary or
server replacement. `RequestScope::Server` survives stream reconnects but rejects
server replacement. These scopes do not replace per-resource versions or live
revision checks. Rejected callbacks are logged at debug level with their issue
identity and current generation/epoch; they never mutate the new state.

Per-resource "latest wins" guards use `Revisions<K>` from the same module
(working diff and task reads per directory, permission mode and usage totals per
session). Every revision comes from one counter, so `forget` on folder removal or
session deletion can never let a pre-removal response alias a fresh request.
Removing a folder also forgets its worktree checkouts' revisions
(`Store::forget_folder` returns the directories it dropped). Saved-grant
refreshes keep a plain counter: it is never forgotten, so it cannot alias.

Debounces and timers are owned GPUI tasks, not counters: the notice auto-clear,
catalog-refresh debounce and settings-save debounce are `Option<Task<()>>` fields
on the controller. Replacing one drops (cancels) the previous timer;
`flush_settings` drops the pending save. `Controller::run` is unfenced and
detached by design: it is for mutations whose outcome is persisted server state
(delete, rename, archive, worktrees, inbox) or local work (opening an editor).
Reads and completions that roll back or show transient state use `run_on`.

`domain/fetch.rs` holds the request status of re-fetchable cached data (saved
grants, folder diffs, usage): `FetchStatus` is `Idle | Loading | Failed`, so
loading and failed are exclusive. The cached value stays beside it, so a refresh
or a failed refresh still shows the last data. Sidecar restart abandons
in-flight fetches and keeps shown errors.

`gh` failures are a typed `GhFailed` with the CLI's stderr; "no pull request" is
classified there (gh has no distinct exit code), never on a formatted error chain.
Controller warnings use structured fields (`op`, `session_id`/`dir`, `generation`,
`error`).

`domain/choice.rs` orders optimistic model/agent choices. The display changes at
once; only the newest outstanding choice of a session may roll it back, and only
to the last value the server is known to hold: the value before the first
outstanding choice, an older choice that succeeded meanwhile, or the latest
`session.model.selected`/`session.agent.selected` the stream reported
(`Controller::product_event` feeds those in; local optimistic applies bypass it).
The stream overwrites the display directly, so it remains authoritative when a
server applies overlapping requests out of order. An exhaustive test covers every
completion order and outcome of three overlapping choices. Session removal and
sidecar restart drop outstanding entries; those completions are discarded by
`run_on(RequestScope::Server)`.

Read demand belongs to its consumer. Mention search and the go-to-file dialog
retain their GPUI task handles, cancelling obsolete debounces/replies on query
replacement or owner drop. Read-only file/search/shell requests use
`controller/reads.rs`: a dropped reply receiver cancels the Tokio future. Mutations
retain their existing completion policy because cancelling a future cannot undo
an already-admitted prompt, approval or write. Blocking demo searches remain
bounded and run to completion. The SSE pump retains explicit abort-on-stop/drop;
process startup/teardown and PTY server lifetimes remain separate owners.

`api/error.rs` preserves HTTP status through anyhow context with the existing
300-character diagnostic bound. Location-startup retry uses typed 404 status, not
occurrences of “404” or “LocationNotFoundError” in unrelated text. File-read 404
and 304 still map to `Missing` and `NotModified`; permissive event/model parsing
is unchanged. Retry closures use concrete futures rather than boxed trait objects.

Tests replay real store fixtures and exercise completion orders, duplicate and
foreign tickets, delete/recreate, refresh failure, disconnect/restart and read
cancellation. A small independent reference model exhaustively checks all
four-command history sequences from cold and cached states (8,192 sequences).
`rustc --edition=2024 --test src/domain/mod.rs -o /tmp/ocdesk-domain-tests` runs the
pure models without GPUI; the same tests run in `cargo test --locked`.

The design follows the mechanism/policy separation in ripgrep's
[matcher](https://github.com/BurntSushi/ripgrep/tree/91f5d22ef48bde459d90a8c7dbeab416e97d0f2b/crates/matcher),
[searcher](https://github.com/BurntSushi/ripgrep/tree/91f5d22ef48bde459d90a8c7dbeab416e97d0f2b/crates/searcher)
and [core](https://github.com/BurntSushi/ripgrep/tree/91f5d22ef48bde459d90a8c7dbeab416e97d0f2b/crates/core):
small concrete mechanisms with explicit contracts, composed by application policy.
It adds no crates, dependencies, universal reducer or generic task framework.
No UI frame-time, memory or throughput improvement is claimed. Rendering, SSE
framing/buffering and transcript merge algorithms are intentionally unchanged;
removing retry boxing only removes its per-attempt allocation, not measured UI cost.
