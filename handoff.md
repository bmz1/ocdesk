# Handoff

State of ocdesk for whoever picks it up next. Read this first, then
[README.md](README.md) and [docs/protocol-notes.md](docs/protocol-notes.md).

_Last updated: 2026-11-07, M6 + M7 pushed on `claude/loving-dirac-otenca`._

## Where things are

- **Done:** M0 (spike), M1 (folders + sessions, streaming conversation,
  composer, reconnect, crash recovery), M2 (incremental streaming
  markdown, code-block copy buttons, links, selectable text, collapsible
  reasoning, frame-time probe, offline demo mode), M3 (expandable tool
  cards, session-scoped permission prompts/replies and latest-turn file
  diffs) and M4 (question forms, Codex-style tool cards, model/agent/variant
  pickers, `@` mentions of files and subagents, prompt history) and M5
  (keyboard shortcuts, persisted window/sidebar/last session/draft model,
  error states), M6 (CI, macOS `.app`/dmg, Linux AppImage/tarball,
  login-shell PATH) and M7 (queue/steer, `/` menu, background tasks
  drawer, git branch + worktrees), targeting
  **opencode 2.2.6 only**. Dark theme by default.
- **Workflow the owner wants:** branch from `master` → feature PR → owner
  reviews → merge to `master`. Don't push to `master` directly; the owner
  opens/asks for PRs. M1–M3 are merged (PRs #1–#3).
- M1–M5 are merged (PRs #1–#5). **M6 + M7** are pushed on
  `claude/loving-dirac-otenca` (branched from `master` @ `41d9595`).
  **No PR opened yet**; the owner asks for it.
- The old `claude/friendly-lamport-ne3gxc` remote branch could not be deleted
  from the agent sandbox (git proxy 403); the owner was asked to delete it.
- **Next:** owner review of M6/M7; watch the first CI run (macOS job and
  the Package workflow have never run). The owner will pick from the
  design review (`docs/design-review.md`, status column = what's done)
  later; don't apply it unasked. Running on a Mac is still the biggest gap.

## Decisions already made (don't re-litigate)

| Decision | Why |
|---|---|
| opencode **2.x only**; 1.x rejected at startup, no v1 code or compat | Owner: "v1 can be thrown away". 2.x is a pure `/api/*` server. |
| Thin reqwest client, no SDK crate | `opencode-codes` targets the 1.x API. `src/api/` is the only HTTP code. |
| One sidecar for all folders | 2.x sessions are pinned to `location.directory`; `/api/event` covers every location. |
| `gpui-kit =0.9.0` (= `gpui-pre 0.3.11` = zed@f816e1f) instead of a Zed git rev | gpui-component must match the gpui snapshot; see docs/gpui-notes.md. |
| Codex-style design, dark default | Owner preference. Take layout ideas, not branding. |
| gpui-component `MessageScroller` for the message list | Virtualized, tail-follow, jump-to-latest built in. |
| Folders persisted in `settings.json` (config dir) | Folders are ocdesk's concept; the server only knows locations/projects. |
| Markdown = gpui-component `TextView`/`TextViewState`, one state per prose block (`ui/prose.rs`) | Its `set_text` → `push_str` already re-parses only the tail, off-thread; selection, copy, links, code-block actions built in. No custom parser. |
| User prompts render as markdown too | Makes them selectable/copyable; the only alternative state format is HTML. |

## How to run and verify (Linux sandbox)

```sh
npm i -g @opencode/cli@2.2.6                    # opencode + opencode2
python3 dev/mock_llm.py &                       # deterministic model on :4555
XDG_CONFIG_HOME=$PWD/dev/xdg cargo run -- /some/git/repo
```

- No GPU/display: `Xvfb :99 -screen 0 1400x900x24 &` and `DISPLAY=:99`.
  wgpu falls back to llvmpipe. Screenshots: `import -window $(xdotool search --name ocdesk) out.png`.
- Drive the UI with `xdotool` (clicks and typing work; window at the
  position `xdotool getwindowgeometry` reports). There is no D-Bus, so the
  native folder picker falls back to the typed-path dialog.
- Debug hooks: `OCDESK_VERIFY_ON_IDLE=1` (logs `verify: store matches REST`
  after each turn: the main correctness check), `OCDESK_PROBE_PROMPT`,
  `OCDESK_EXIT_ON_IDLE`, `OCDESK_DEBUG_PANIC`, `OCDESK_FOLDER_DIALOG`,
  `OCDESK_THEME`, `OCDESK_OPENCODE`.
- Checks before every commit: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
  and `DISPLAY=:99 ./dev/teardown-test.sh` (sidecar dies on TERM/INT/KILL/panic/quit).
- The sidecar process shows up as `…/opencode2 serve --hostname 127.0.0.1 --port 0 --print-logs`
  (match `pgrep -f "serve --hostname"`; `"opencode serve"` doesn't match).
- A clean build of the gpui stack takes about 10 minutes; incremental builds are seconds.
- Sandbox note: installing new npm packages may be blocked as "external
  code" until the owner approves (that happened for `@opencode/cli`).

**Without opencode** (e.g. when npm installs are blocked): `OCDESK_DEMO=<turns>`
runs the UI on synthetic history and streams replies locally (README,
*Developing*). M2 was mostly verified this way; after the owner approved the
`@opencode/cli@2.2.6` install it was also run against the real server: a
`rich` prompt (mock model) renders fully, `OCDESK_VERIFY_ON_IDLE` logs
`verify: store matches REST`, and `dev/teardown-test.sh` passes 5/5.
Under Xvfb rendering also needs `mesa-vulkan-drivers` (else "Failed to create
surface"); `xclip -o -selection clipboard` checks copy.

Verified at the end of M2 (Linux/Xvfb/llvmpipe, demo mode, debug build):
markdown (headings, emphasis, inline code, links, quotes, nested/ordered/task
lists, tables, fenced code) streams in; code copy button and drag-select +
Ctrl-C put the right text on the clipboard; reasoning rows start collapsed
and expand in place. 500-message session: streaming a long reply 3–5 ms mean
frame CPU time (p95 ≤12 ms); scrolling up through ~300 messages 5–6 ms mean,
max <9 ms; peak RSS 180 MB idle, 210 MB after that scroll. Release build:
streaming 2.6–3.9 ms mean, scrolling the whole 500-message history 4.6–5.5 ms
mean (max 10.7 ms); peak RSS **175 MB idle, 207 MB after scrolling
everything, slightly over the 200 MB target**. llvmpipe keeps GPU textures and
buffers in process memory, so real-GPU RSS should be lower; measure on a Mac
before optimizing (candidates: drop parsed states for rows far off-screen
sooner than the 400-entry LRU cap, or lower the cap). llvmpipe caps presentation at ~20–30 fps, so frame
*rate* here says nothing; the CPU time is the number to compare.

Verified at the end of M1 (Linux/Xvfb, mock model, opencode 2.2.6): tool turn and
Stop match REST; two sessions in two folders streaming in parallel with
switching; rename and delete via dialogs; `kill -9` of the sidecar recovers;
teardown 5/5; a 1.x binary is refused with an install hint. **Nothing has run
on macOS yet**, and macOS is the primary target.

Verified at the end of M3 (Rust 1.99.1, Linux/Xvfb/llvmpipe,
real opencode 2.2.6 + deterministic mock model):
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo test`: **21 passed** (including M2's prose tests and an HTTP wire
  test for permission paths, all reply decisions, 204s, failure and diffs).
- `python3 dev/m3-protocol-test.py <opencode2>`: once / reject / always,
  pending-permission recovery via REST, rejected tool history, edit + diff.
- Native UI: Allow once and Reject shell requests; Always allow an edit;
  tool input/output expansion, Copy output pasted back into the composer;
  a colored edit preview with visible actions; latest-turn file diff.
- `OCDESK_VERIFY_ON_IDLE=1`: store matches REST after shell approval,
  rejection and edit approval. `dev/teardown-test.sh`: **5/5**.
- Screenshots: `docs/screenshots/m3-{tool,permission,changes}.png`.
- macOS remains untested. M3's full UI reconnect/failure scenarios were
  not driven manually; protocol/store tests cover request recovery and
  revisions, while existing M1 reconnect behavior is preserved.

## Code map

```
src/main.rs            logging, tokio runtime, signal handling, window
src/shell_env.rs       login-shell PATH when launched outside a terminal
src/sidecar.rs         spawn/health/kill opencode; version gate (>= 2.x)
src/settings.rs        folders, collapsed, theme → settings.json
src/api/mod.rs         HTTP client + SSE reader (heartbeat timeout, backoff)
src/api/model.rs       SessionInfo, Message, Block (lenient JSON)
src/api/events.rs      /api/event → Event (execution lifecycle, blocks by ordinal)
src/store.rs           Entity<Store>; apply(event), reconcile(REST snapshot)
src/controller.rs      intents, sidecar lifecycle, auto-restart, pump, folders
src/ui/*               app_view (layout, window/sidebar persistence), keys (shortcuts),
                       sidebar (folders), conversation, message, composer
src/ui/prose.rs        ProseCache: one TextViewState per prose block, LRU-capped
src/ui/frame_stats.rs  OCDESK_FRAME_LOG frame-time + peak-RSS probe
src/ui/tool_card.rs    expandable input/output, errors and optional edit diffs
src/ui/keys.rs         app-wide shortcuts + shortcuts sheet
src/ui/tasks.rs        background tasks drawer (subagents, shells)
src/ui/git.rs          branch pill, worktree dialogs
src/ui/icons.rs        extra Lucide icons + AppAssets
packaging/             icon, macOS bundle.sh + Info.plist, Linux package.sh
.github/workflows/     ci.yml (checks + e2e), package.yml (installers)
dev/harness.py         real opencode serve + mock model, for protocol probes
src/ui/permission.rs   pending permission queue and explicit reply controls
src/ui/changes.rs      read-only latest-turn file diffs
src/demo.rs            OCDESK_DEMO offline mode (synthetic history + streaming)
tests/fixtures/        real 2.2.6 captures (events.jsonl + messages.json)
```

Data flow: view → `Controller` intent → `Api` on Tokio → result back via
`cx.spawn` → `Store` → `StoreEvent` → views re-measure only changed rows.

## Next steps

**M2 leftovers**
- Run M2 on macOS (primary target, nothing has run there yet). On Linux it
  has run against real opencode 2.2.6 (one `rich` turn, verify + teardown). Measure frame times on
  real hardware with `OCDESK_FRAME_LOG=1` in a release build.
- No syntax highlighting in code blocks: gpui-kit's `syntect` feature
  (or gpui-component's tree-sitter languages) would plug into `TextView`
  automatically via its code-block highlighter; it costs build time and was
  left out.
- Each prose block is its own `TextViewState`. Selecting within a block is
  verified; a drag across two messages was not tested (gpui-base's Copy
  handler reads a window-level `TextSelection`, so it may already work).
- Memory: 207 MB peak in the sandbox (see above), just over target.
- `ProseCache` is cleared on session switch, so switching back re-parses the
  visible rows (cheap: small replacements parse synchronously). It's capped
  at 400 states with LRU eviction.

**Small follow-ups found during M1**
- The sidebar error footer doesn't wrap long paths (M5 error states).
- When the sidecar crashes mid-turn, the partial assistant message has no
  "interrupted" marker until the server writes one (it may resume the turn
  on restart; 2.x has crash recovery for claimed executions).
- The sidebar list isn't virtualized (fine for hundreds of sessions).
- `RetryScheduled` is decoded but the payload shape wasn't captured live.

**M3 notes**
- Tool input/output is expandable and copyable. Preview sizes are bounded;
  copying keeps the full returned text. Terminal error output, exit codes,
  server truncation flags and optional `metadata.diff` are preserved.
- Permissions are independent of loaded history, so background sessions get
  waiting badges. Explicit once/always/reject replies use the session-owned
  endpoint. Save rules are displayed for Always allow. Failed replies remain
  retryable; disconnected replies are disabled. No automatic approvals.
- Reconnect/selection restores pending requests through REST. Per-session
  revisions reject stale snapshots, and sidecar generations/stream epochs
  reject stale reply callbacks. A reply from another client removes the card.
- Changes displays the latest turn's per-file patches (`context=3`), counts,
  server truncation and copy. Refresh during a turn; an opened view refreshes
  when the turn ends. It is not a full-session/repository diff.
- `form.*` questions are implemented in M4 (`ui/form_view.rs`), separate from permissions.
- The mock write tool now sends `path`, as required by 2.2.6, rather than the
  old `filePath` argument. `dev/m3-protocol-test.py` checks real once/always/
  reject permission flow, restoring pending requests and an edit diff in a
  temporary repository with isolated config/data.

**M4 notes**
- Verified on Fedora/Wayland (real GPU, not Xvfb; no xdotool here):
  `cargo fmt --check`, `clippy -D warnings`, `cargo test` **30 passed**
  (incl. an HTTP contract test for model/agent/find/switch/prompt-attachment
  wire shapes, mention/history unit tests, store precedence test). Real
  opencode 2.2.6: `types=chat`, `fs/find`, switch endpoints/events and
  `files`+`agents` attachments with UTF-16 mention offsets (scratch probe);
  app probe prompt → `verify: store matches REST`.
- Driven by hand in M5 (Xvfb+xdotool, real opencode 2.2.6 + mock model):
  `@mai` → popup, ↓/Enter picks `README.md` as a chip, the reply shows the
  attached content, `verify: store matches REST`; ↑ recalls the prompt; the
  model menu opens. Found and fixed: the model menu repeated a provider
  heading because the server doesn't list models grouped by provider (now
  grouped stably by first appearance). Screenshot `docs/screenshots/m4-mention.png`.
- Key interception: GPUI dispatches key *bindings* before key listeners, so
  the composer claims Up/Down/Enter/Tab/Escape with `capture_action` on the
  textarea's actions (`MoveUp`, `Enter`, `IndentInline`, …), not
  `on_key_down`.
- Mentions are gpui-base inline tokens (`replace_range_with_token`), id
  `file:<rel>` / `agent:<id>`; `ui/mention.rs::build_prompt` turns
  `content().tokens()` into attachments.
- Ideas left: directory mentions (server behavior for a dir `file://` URI
  is untested), restoring tokens on history recall, showing attached files
  as chips on sent user messages (the projection has `files[]`), `skills`
  attachments, persisting the draft model/agent in settings (M5).
**M5 notes**
- Keys (`ui/keys.rs`, window-level bindings, no key context): Mod+N/O/B/L/./
  /, Mod+Shift+D, Mod+Q, Alt+↑/↓. Inputs' context bindings win on conflict;
  the only clash, `secondary-.` (gpui-base `ToggleCodeActions`), is claimed
  in the composer with `capture_action`. Esc in the composer stops a running
  turn when the `@` popup isn't open. `AppView` holds a fallback
  `FocusHandle` and focuses it when nothing is focused, so shortcuts dispatch
  after clicking non-focusable UI.
- `Settings` gained `window`, `sidebar_width`, `sidebar_hidden`,
  `last_session`, `draft_model`, `draft_agent` (all `#[serde(default)]`, old
  files load). `Controller::update_settings` debounces writes by 500 ms;
  `flush_settings` runs from `on_app_quit`. Window bounds come from
  `observe_window_bounds` (restore bounds when maximized); a saved position
  that's on no display falls back to centered. `last_session` is restored
  after the first session list, only when no folder argument was given.
- Error states: sidebar footer error is width-bounded and scrolls, with
  Copy details (message + sidecar's last lines); a banner above the composer
  when reconnecting or opencode isn't running (with Restart); a failed
  history load shows the error + Retry in place of the list
  (`Store::load_errors`), and "Loading conversation…" while empty and
  unloaded. The not-installed hint pointed at the 1.x installer; now
  `sidecar::INSTALL_HINT` (`npm i -g @opencode/cli@2.2.6`).
- Verified (Linux/Xvfb/llvmpipe, real opencode 2.2.6 + mock model): fmt,
  clippy `-D warnings`, `cargo test` **35 passed**, teardown **5/5**. By hand:
  shortcuts sheet, Mod+B, Mod+N + send, Alt+↓, drag-resize of the sidebar,
  Esc stops a `long` turn ("Stopped"), Mod+Q quits and kills the sidecar,
  settings round-trip (window 1000×700 @ 50,60 restored; sidebar width; last
  session reopened), missing-opencode state
  (`docs/screenshots/m5-sidecar-missing.png`, `m5-shortcuts.png`).
- Not driven: the history-load Retry panel (needs a failing
  `GET /api/session/{id}/message`; logic is unit-tested), maximized restore
  (no window manager under Xvfb), macOS Cmd bindings.
- Ideas left: an "interrupted" marker for turns cut off by a sidecar crash;
  a command palette; per-session composer drafts.

**M6 notes**
- **Toolchain is 1.99.1** (`rust-toolchain.toml`; CI pins the same).
  gpui-pre-macos 0.3.11 calls `Option::map_or_default`, unstable on 1.97,
  so the old pin could never build on a Mac. `cargo check`/`clippy` pass
  for `aarch64-apple-darwin` (type-check only: the gpui Metal build script
  needs Xcode; I touched an empty `stitched_shaders.metal` in `target/`).
  Nothing has been linked or run on macOS.
- `src/shell_env.rs`: when stdin isn't a terminal (Finder/Dock/desktop
  launch), run `$SHELL -l -i -c` once at the top of `main` (5 s timeout,
  markers, fish handled) and prepend its PATH. `OCDESK_NO_SHELL_ENV=1`
  opts out. Verified detached with `PATH=/usr/bin:/bin`.
- App id `io.github.bmz1.ocdesk` (window `app_id`, `.desktop`, bundle id).
  Icon `packaging/icon.png` is generated; scripts derive all sizes.
- `packaging/linux/package.sh`: stripped tarball (+ install.sh, desktop
  entry, hicolor icons; tested here) and AppImage via linuxdeploy (running
  the downloaded tool was blocked in the sandbox; first run will be CI;
  `SKIP_APPIMAGE=1` skips it). `packaging/macos/bundle.sh`: universal
  `.app`, icns, ad-hoc or Developer ID signing, optional notarization, zip
  + dmg. **Never run.**
- CI: `ci.yml` (Linux fmt/clippy/test/shellcheck/build + opencode 2.2.6
  e2e: `m3-protocol-test.py`, `teardown-test.sh`; macOS clippy/test/build)
  only on `master` pushes and PRs. `package.yml` on `v*` tags (draft
  release, SHA256SUMS) or manual; signing secrets optional (README).

**M7 notes** (all probed live first; shapes in docs/protocol-notes.md, *M7*)
- `dev/harness.py` boots a real `opencode serve` + mock model in a temp
  repo. Mock keywords added: `tool:task` (subagent tool), `tool:sleep`
  (long shell), `slow` (10 s stream).
- **Queue/steer**: `delivery` on prompt; per-session inbox in the store
  (`session.inbox.*`, `GET …/inbox`); a user prompt enters the transcript
  on *delivered*, not enqueued. Enter steers while busy, Ctrl/Cmd+Enter
  queues; pending stack above the composer (Steer now / edit / remove).
  **Transcript order is delivery order, not id order**: store lookups
  search from the end, `Api::messages` and reconcile keep server order
  (the old id sort hid this from `OCDESK_VERIFY_ON_IDLE`).
- Composer: per-session drafts, Esc twice to stop, busy placeholder.
- **`/` menu** (first word only): `/compact`, server commands
  (`POST …/command {name, text}`), skills (token → `skills` attachment),
  MCP servers (toggle; untested, no server configured). Compaction/skill
  entries are `Message::Marker` dividers; no stream event describes them,
  so those sessions are re-fetched at turn end (`Store::needs_reload`).
- **Background tasks drawer** (`ui/tasks.rs`, Mod+Shift+T): child sessions
  (`parentID`) and shells (`shell.*`, output polled 1 s, Stop = DELETE).
  Subagent calls / backgrounded shells are one row in the transcript.
  Overlays below a 1440 px window. Subagent result shown as plain text.
- **Git** (`ui/git.rs`): branch pill (start session in / move session /
  New worktree…), worktrees as sidebar sub-groups, remove asks on
  `forceRequired` (`api::WorktreeDirty`). opencode creates worktrees
  **detached** and only checks out existing branches, so the pill says
  "detached". Worktrees reuse their repo folder's catalog/`/` menu.
- Gotcha: `use super::*` in a test module of a file with
  `use gpui_kit::*` imports gpui's `test` attribute; import explicitly.
- Verified (Linux/Xvfb, real 2.2.6 + mock): fmt, clippy `-D warnings`,
  `cargo test` **51 passed**; by hand: queue+steer order, Steer now,
  remove, double Esc, `/review`, `/compact` (failure marker), skill token,
  subagent row + drawer, shell drawer + Stop, worktree create / session /
  move / remove with force; `verify: store matches REST` after each.

