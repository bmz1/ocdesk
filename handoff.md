# Handoff

State of ocdesk for whoever picks it up next. Read this first, then
[README.md](README.md) and [docs/protocol-notes.md](docs/protocol-notes.md).

_Last updated: 2026-11-07, end of M1 (PR #1, branch `m1-sessions-conversation`)._

## Where things are

- **Done:** M0 (spike) and M1 (folders + sessions, streaming conversation,
  composer, reconnect, crash recovery), targeting **opencode 2.2.6 only**.
  Dark theme by default.
- **Workflow the owner wants:** branch from `master` → feature PR → owner
  reviews → merge to `master`. Don't push to `master` directly. PR #1 holds
  M1 and was **not merged yet** at the time of writing; check its state first.
- The old `claude/friendly-lamport-ne3gxc` remote branch could not be deleted
  from the agent sandbox (git proxy 403); the owner was asked to delete it.
- **Next milestone: M2** (see *Next steps*).

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
tests/fixtures/        real 2.2.6 captures (events.jsonl + messages.json)
```

Data flow: view → `Controller` intent → `Api` on Tokio → result back via
`cx.spawn` → `Store` → `StoreEvent` → views re-measure only changed rows.

## Next steps

**M2 (make-or-break): markdown + text**
1. Incremental streaming markdown in `ui/message.rs`: parse only the tail of
   a streaming block (blocks have `done`; finished blocks can cache their
   parsed form). gpui-component has `text::TextView` / markdown in 0.9.0 (see
   the gpui-kit `examples/markdown`); check whether it can be fed
   incrementally before writing our own.
2. Fenced code blocks with a copy button; links open in the browser.
3. Collapsible reasoning, collapsed by default (Codex-style "Thinking" row).
4. Selectable, copyable text.
5. Perf: a 500-message session at display refresh rate; log frame times in
   debug builds; memory under 200 MB. Measure on real hardware: llvmpipe
   numbers aren't representative.

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
