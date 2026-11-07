# Handoff

State of ocdesk for whoever picks it up next. Read this first, then
[README.md](README.md) and [docs/protocol-notes.md](docs/protocol-notes.md).

_Last updated: 2026-11-07, end of M2 (branch `claude/elegant-mccarthy-dymj86`, no PR opened yet)._

## Where things are

- **Done:** M0 (spike), M1 (folders + sessions, streaming conversation,
  composer, reconnect, crash recovery) and M2 (incremental streaming
  markdown, code-block copy buttons, links, selectable text, collapsible
  reasoning, frame-time probe, offline demo mode), targeting **opencode
  2.2.6 only**. Dark theme by default.
- **Workflow the owner wants:** branch from `master` → feature PR → owner
  reviews → merge to `master`. Don't push to `master` directly. PR #1 (M1)
  is merged. M2 is on `claude/elegant-mccarthy-dymj86`; a PR for it was not
  opened yet (the owner opens/asks for PRs).
- The old `claude/friendly-lamport-ne3gxc` remote branch could not be deleted
  from the agent sandbox (git proxy 403); the owner was asked to delete it.
- **Next milestone: M3** (see *Next steps*).

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
*Developing*). M2 was verified only this way: the sandbox refused to install
`@opencode/cli`, so **M2 has not been run against a real opencode server**.
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

## Code map

```
src/main.rs            logging, tokio runtime, signal handling, window
src/sidecar.rs         spawn/health/kill opencode; version gate (>= 2.x)
src/settings.rs        folders, collapsed, theme → settings.json
src/api/mod.rs         HTTP client + SSE reader (heartbeat timeout, backoff)
src/api/model.rs       SessionInfo, Message, Block (lenient JSON)
src/api/events.rs      /api/event → Event (execution lifecycle, blocks by ordinal)
src/store.rs           Entity<Store>; apply(event), reconcile(REST snapshot)
src/controller.rs      intents, sidecar lifecycle, auto-restart, pump, folders
src/ui/*               app_view, sidebar (folders), conversation, message, composer
src/ui/prose.rs        ProseCache: one TextViewState per prose block, LRU-capped
src/ui/frame_stats.rs  OCDESK_FRAME_LOG frame-time + peak-RSS probe
src/demo.rs            OCDESK_DEMO offline mode (synthetic history + streaming)
tests/fixtures/        real 2.2.6 captures (events.jsonl + messages.json)
```

Data flow: view → `Controller` intent → `Api` on Tokio → result back via
`cx.spawn` → `Store` → `StoreEvent` → views re-measure only changed rows.

## Next steps

**M2 leftovers**
- Run M2 against real opencode 2.2.6 (only demo mode was exercised) and on
  macOS (primary target, nothing has run there yet). Measure frame times on
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

**M3:** tool cards; permissions via `permission.asked` and
`POST /api/session/{id}/permission/{requestID}/reply {decision}`; diffs.
2.x also has `form.*` events (questions to the user).
**M4:** model picker (`/api/model`, `/api/model/default`, `POST /api/session/{id}/model`),
agents (`/api/agent`), `@` mentions (`/api/fs/find`), prompt `files`.
**M5/M6:** see README roadmap.
