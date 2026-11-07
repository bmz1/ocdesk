# UX research: Codex, Claude desktop, T3 Code, Rune (2026-11-07)

What the leading desktop coding-agent clients do, what Anomaly's Rune/rune-kit
ships for an opencode desktop, and what ocdesk should change in its UI
components. Complements [design-review.md](design-review.md) (whose open items
are referenced as **DR#n**); nothing here is applied yet.

Sources: the T3 Code repo (`pingdotgg/t3code` @ 2026-11-07, `apps/web/src/components`,
`docs/user/*`), the Rune repo (`anomalyco/rune` `dev` @ `65e7e60`, `crates/rune-kit`,
`PLAN.md`, `demo/opencode/*`), Claude Code desktop docs
(code.claude.com/docs/en/desktop) and the Codex app docs (learn.chatgpt.com/docs/app,
/features, /code-review, /environments/git-worktrees). The Codex pages don't
describe tool-call rendering or the model picker; those rows below come from the
other three.

## 1. Where everyone converged

These patterns appear in at least three of the four references, so treat them as
table stakes rather than taste:

| Pattern | Codex | Claude desktop | T3 Code | Rune opencode demo | ocdesk today |
|---|---|---|---|---|---|
| Collapsed work log between prose ("Used 3 tools ›", "Ran 2 commands, edited 3 files") | ✓ | Normal / Thinking / Verbose (Ctrl+O) | ✓ + Hide successful / Zen | ✓ | Groups consecutive calls (`render_group`), summary string, no density setting |
| Reasoning row with duration ("Thought for 4s") | – | ✓ | ✓ | ✓ | "Thought ›", no duration, one row per reasoning part |
| Approval actions in the composer slot, keyboard-driven | ✓ | Allow once / Always allow / Deny | `ComposerPendingApprovalActions`; typed reply = deny with feedback | – | Card in the transcript, equal-weight buttons (DR#2) |
| Permission mode next to model, default hidden | ✓ (below composer) | ✓ (Cmd+Shift+M) | shown only when not default | – | Separate control, always shown |
| Composer bottom row: agent · model · effort as quiet dropdowns; send ↔ stop | ✓ | ✓ (+ usage ring) | ✓ | `Build ⌄  Claude Opus 5.5 ⌄  High ⌄` | Present, heavier styling |
| Context/tokens/cost always visible | – | usage ring + session rail card | `ContextWindowMeter` | header: ring · 64.6K · $1.84 | Details popover only (`usage.rs`) |
| Command palette Cmd/Ctrl+K | search chats ⌘K | Cmd+/ shows all | `CommandPalette*` | "Search sessions" row | None |
| Sidebar status vocabulary (running / needs you / unread / error) | Activity inbox with filters | – | `ThreadStatusIndicators`, snooze, settle | blue/amber dots, project monograms | spinner + "! n", unread dot |
| Pin + archive, with Undo | pin | archive, auto-archive on PR merge | pin/snooze/settle/archive, 5 s Undo toast | – | rename/delete only |
| Diff pane beside the chat, with scopes and inline comments → next prompt | Unstaged/Staged/Commit/Branch/Last turn; stage/revert hunks; inline comments | file list + per-file; `+` on a line to comment; Code review card | `DiffPanel`, `ChangedFilesTree`, open-in-editor | split + unified diff component, file tree with `+12 −3` | Full-pane toggle (Cmd+Shift+D) replacing the conversation |
| Right-side panel with tabs (Diff / Tasks / Terminal / Plan) | review pane, terminal | rearrangeable panes | `RightPanelTabs`, `ThreadTerminalDrawer` | `SplitView` inspector | Tasks drawer; Changes replaces main view |
| Find in thread (Cmd+F) | – | – | `ThreadFindBar` | `Tx::scroll_center` to text highlights | None |
| Keyboard focus areas (F6), list arrows, F2 rename | shortcuts page | Ctrl+Tab between sessions | F6 / Shift+F6, arrows, F2, Shift+F10 menu | Sidebar is one Tab stop, arrows, typeahead | Alt+Up/Down only |

## 2. Rune / rune-kit: what it is and what it means for ocdesk

- **What:** Anomaly's (opencode's maker) GPU-rendered Rust UI framework (wgpu,
  Parley text, Taffy, AccessKit, springs), plus `rune-kit`, components named after
  `@opencode/ui`, plus `themes/opencode` with every `@opencode/ui` token as data
  (`assets/theme.json`, Inter + IBM Plex Mono). MIT.
- **Stated goal (PLAN.md §10 M3):** "OpenCode Desktop's main screens": sidebar,
  streaming transcript, composer, diff, command palette, against a live opencode
  server. `demo/opencode/*.png` already shows it. That's the same product
  space as ocdesk, from the upstream team.
- **Why they didn't use GPUI (PLAN.md §1):** no compositing or blur, animations
  re-render on the main thread, whole-window redraw, re-entrant borrow panics.
  ocdesk hasn't run into any of these yet. Its measured frame times are fine
  (handoff: 2.6–3.9 ms streaming, release).
- **Maturity:** "pre-release, pin a rev of `dev`", no releases, **no external
  PRs** ("have your agent open an issue"), IME/text input listed as the top risk,
  Linux/Windows keep the system titlebar, and the API changes weekly
  (CHANGELOG).

**Recommendation: don't port now, but adopt three things from it.**

1. **Its design tokens.** Generate an ocdesk gpui-component theme from
   `themes/opencode/assets/theme.json` (both appearances) and bundle Inter + IBM
   Plex Mono (OFL). ocdesk then looks like opencode instead of like a default
   gpui-component app, at low cost. Do it as a small build-time or
   one-off codegen into `src/ui/theme.rs`.
2. **Its component contracts as our internal API.** Introduce `src/ui/kit/`
   with the same names and builders where they overlap (`SidebarRow`,
   `PaneState`, `SplitView`-style inspector, `Combobox`/`PopupList`, `Tree` with
   `changes(+,−)`, `Toast` with action, `Keybind`). Today each view hand-builds
   rows with `h_flex()` chains. With a kit layer, ocdesk gets consistency now,
   and porting to Rune later becomes a backend swap instead of a rewrite.
3. **Its sidebar and transcript measurements**, which it documents as the
   agent-chat preset: sidebar 276 pt (range 136–640) with collapse-by-drag
   and double-click reset; 45 pt rows with a project monogram and a muted
   project subtitle; a status that becomes a close button under the pointer;
   and, in the opencode demo, "Thought for Ns" and
   "Used N tools ›" as single muted lines; a copy icon under the final reply;
   a footer strip with the folder path.

**Revisit trigger:** when Rune tags a release or its M3 opencode screens ship,
spend one day on a spike that renders ocdesk's `Store` through rune-kit headlessly
(`--snap` PNG + element tree). That headless harness is also worth copying as an
idea now: ocdesk's Xvfb + xdotool verification is the slowest part of its loop.

## 3. Proposed changes, prioritised

Each item names the files it touches. P0 is the transcript, which users look at
all day. P1 is navigation. P2 is review and panes.

### P0: the transcript reads as one turn, not fragments

**1. Work log, Codex/T3 style** (`tool_card.rs`, `message.rs`, `settings.rs`)
- Collapse *all* tool calls between two prose blocks into one row, not only
  consecutive same-kind runs. Summary rules from T3's activity log: at most two
  categories plus "and N more"; commands and edits rank above reads and searches;
  failures counted separately in danger colour ("· 1 failed").
- While running, the collapsed row shows the *current* call's one-liner
  (`$ cargo test`, `Editing src/store.rs`) with a spinner. Once the turn ends it
  settles to the summary.
- Per-tool verbs and icons (DR#3): Ran `$ cmd`, Edited `path +12 −3`, Read
  `path`, Searched `pattern`, Fetched `host`. Terminal-style output, trailing
  blank lines trimmed, JSON input behind "Raw input".
- **Transcript density** setting (Claude: Normal / Thinking / Verbose; T3: Show
  all / Hide successful / Zen), also toggled with Ctrl+O. Approvals, questions and
  errors always show.

**2. Reasoning** (`message.rs::reasoning`)
- "Thought for 4s" using part timestamps. Merge adjacent reasoning parts in one
  turn into one row (DR#4). Keep it collapsed by default. While streaming, show a
  one-line live preview (T3 `remarkThoughtPreview`).

**3. Turn footer** (`message.rs`, `controller.rs`)
- On hover under the final assistant message: Copy, duration, model, tokens, and
  **Fork from here** (`POST /api/session/{id}/fork`). On the user message: **Undo
  turn** (`/revert/stage` then `/revert/commit`, as T3's timeline rewind and Claude
  Code's `/rewind` do). On error: Retry and Copy error in a danger-tinted card.

**4. Approvals live in the composer slot** (`permission.rs`, `composer.rs`)
- While a request is pending, the composer area shows the request (`$ command`,
  or a trimmed diff capped near 320 px) with **Allow once** (primary, Enter),
  **Always allow** (shows the exact rule; a `*` wildcard in warning colour), and
  **Deny** (Esc). Typing text and sending denies with that text as feedback (T3).
- Subagent requests show in the parent thread with the subagent's name, so
  nobody has to open the tasks drawer to unblock it.

**5. Persistent context meter** (`app_view.rs` header, `usage.rs`)
- Header right side: context ring + tokens + cost (Rune shows `◔ 64.6K $1.84`).
  Click opens the existing usage details. Ring turns warning at 80 % with a
  "Compact" action (`POST /compact`).

### P1: navigation

**6. Command palette, Cmd/Ctrl+K** (new `ui/palette.rs`, reusing the `mention.rs` popup list)
- Sections: Threads (fuzzy over all folders), Folders, Actions (every `keys.rs`
  action, showing its binding), `/` commands, Theme. Number keys pick entries
  while open (T3). Closing returns focus to the composer.

**7. Sidebar** (`sidebar.rs`)
- Status vocabulary with one slot per row: spinner = running, amber dot = needs
  you (approval/question), blue dot = unread, red = failed. Replaces "! 1"
  (DR quick win).
- On hover, the status slot becomes a close/archive button (Rune), so relative
  times stay aligned.
- Project monogram (coloured initial) when the list is flat or filtered.
- **Pin** and **Archive** with a 5 s **Undo** toast; Cmd+Z triggers the last Undo
  (T3). Date sections (Today / Yesterday / Earlier) inside a folder.
- "Search sessions" row at the top (opens the palette pre-filtered) and an
  **Activity** filter for threads that need you (Codex).
- Keyboard: the list is one Tab stop; arrows move, Enter opens, F2 renames,
  typeahead jumps, Shift+F10 opens the row menu.
- Collapsible sidebar with hover peek from the left edge (Rune `SidebarPeek`).

**8. Find in thread, Cmd/Ctrl+F** (`conversation.rs`)
- Bar at the top of the transcript; matches highlighted in prose and tool
  output; Enter / Shift+Enter step through; it expands collapsed work logs that
  contain a match.

**9. Composer row** (`composer.rs`)
- Bottom row, left to right: `+` attach · Agent ⌄ · Model ⌄ · Effort ⌄ ·
  (permission mode only when not the default) … send/stop. All quiet text
  buttons, not bordered selects.
- Direct shortcuts: Cmd+Shift+M mode, Cmd+Shift+I model, Cmd+Shift+E effort
  (Claude's bindings).
- Below the composer, a status strip: folder path (click = open in editor/
  Finder), branch / worktree pill, sidecar status dot. This moves "opencode 2.2.6"
  out of the sidebar footer.
- Large paste becomes an attachment (done); add Cmd+Shift+V to paste inline anyway (T3).

**10. Focus areas** (`keys.rs`, `app_view.rs`)
- F6 / Shift+F6 cycles sidebar → transcript → composer → right panel and
  restores each area's last focus. Ctrl+Tab / Ctrl+Shift+Tab switches threads
  (Claude), in addition to Alt+Up/Down.

### P2: review and panes

**11. Right panel instead of view toggles** (`app_view.rs`, `changes.rs`, `tasks.rs`)
- An inspector pane with tabs **Changes · Tasks · Terminal**, resizable and
  remembered per window. Cmd+Shift+D opens it on Changes instead of replacing the
  conversation, so the diff and the agent are visible side by side. All four
  references do this.

**12. Diff review** (`changes.rs`)
- Scope selector: **Last turn** (`/session/{id}/diff`) · **Working tree** ·
  **Branch** (`/vcs/diff`), modelled on Codex's scopes.
- File tree with `+N −M` per file (rune-kit `Tree::changes`). Unified or split
  view, line numbers, tinted rows, syntax colour, collapsible files.
- **Inline comments:** a `+` on a line's gutter opens a comment, and "Send N
  comments" composes one prompt with `path:line` references (Codex, Claude). It
  only writes a prompt, so it stays clear of the excluded commit/push work.
- Stage/revert hunk is a git write. Ask the owner first, since item 9 was
  excluded.

**13. Plan card** (`message.rs`)
- When the `plan` agent finishes a turn, show its final message as a card with
  **Implement** (switches agent to Build and sends "Implement the plan"), and
  **Copy** (T3 `ProposedPlanCard`).

**14. Terminal tab** (later; `api/` + new `ui/terminal.rs`)
- opencode 2.2.6 exposes `/api/pty` and `/api/pty/{id}/connect`. A per-thread
  terminal in the right panel matches Codex, Claude and T3. It needs a terminal
  emulator crate (`alacritty_terminal`, as Zed uses), so it's sized as its own milestone.

### Platform and visual polish (carry-overs worth bundling)

- macOS: transparent titlebar, traffic-light inset, header as drag region, menu
  bar (DR#1). This is the most visible "not native" tell.
- Syntax highlighting in code fences and diffs (gpui-component's tree-sitter
  highlighter). All three web clients and Rune have it.
- opencode theme tokens + Inter / IBM Plex Mono (section 2.1).
- Relative-time ticker, ≥24 px hit targets, visible focus rings (DR quick wins).

## 4. Suggested sequencing

| Batch | Items | Why first |
|---|---|---|
| A | 1, 2, 3, 4, 5 + syntax highlighting | The transcript and approvals are what every turn looks like |
| B | `ui/kit` layer + opencode tokens/fonts | Do it before 6–12 so new components are built on it once |
| C | 6, 7, 8, 9, 10 | Navigation scales with thread count |
| D | 11, 12, 13 + macOS chrome | Review loop and native feel |
| E | 14, Rune spike when it releases | Larger, external dependency |

## 4b. Implementation status (2026-11-07)

Implemented on `claude/amazing-brahmagupta-qb6r76`, checked with unit tests
and demo-mode screenshots (Linux/Xvfb). Nothing was run against a real
opencode server in this session (installing `@opencode/cli` was blocked), so
items that depend on server behaviour are marked *unverified live*.

| # | Item | Status |
|---|---|---|
| 1 | Work log: ranking, two categories + "and N more", running call named, density (Normal / Verbose / Quiet), raw input toggle | Done |
| 2 | Reasoning merged, "Thought for Ns", live preview | Done |
| 3 | Turn footer (Copy, duration, model, tokens, cost, Fork), Undo from a prompt, Retry/Copy on errors | Done; Fork and Undo *unverified live* (`/fork`, `/revert/stage`+`/commit`) |
| 4 | Approvals above the composer, primary Allow once, `$ command`, wildcard warning, subagent requests in parent, Enter/Esc/typed-reply | Done; reply `message` *unverified live* |
| 5 | Header context ring, tokens, cost, Compact at 80 % | Done |
| 6 | Command palette (sessions, folders, actions with shortcuts, `/` commands, view, models) | Done |
| 7 | Sidebar status slot, pin, archive with Undo toast, date sections, Show more, Needs you view, search row, arrows/Enter/Delete/P, shortcut hints, 60 s time ticker | Done. Not done: hover peek of a hidden sidebar, typeahead, Shift+F10 row menu, Cmd+Z for the last Undo |
| 8 | Find in conversation; matching work logs open | Done (row-level highlight, not in-text) |
| 9 | Composer: quiet selects, permission named only off default, status strip, paste-as-text, picker shortcuts | Done (effort is Mod+Shift+R; Mod+Shift+E conflicts with text-field bindings) |
| 10 | F6 focus areas, Ctrl+Tab | Done |
| 11 | Right panel with Changes / Tasks / Terminal tabs, resizable, persisted | Done |
| 12 | Diff: scopes, collapsible files with counts, line numbers, tinted rows, inline comments → one prompt | Done. Stage/revert hunk left out (git writes; owner's call) |
| 13 | Plan bar with Implement (switches to build, then prompts) | Done |
| 14 | Terminal tab on opencode's persistent PTY | Done; protocol tested against a mock server built from opencode's client source, *unverified live* |
| — | macOS transparent title bar, traffic-light inset, drag region, menu bar, Dock reopen | Done; *unverified on a Mac* |
| — | `ui/kit` components, opencode tokens, Inter / IBM Plex Mono, syntect highlighting | Done: code fences, diffs (review panel, approvals, edit cards; both sides highlighted as continuous text), file reads (line-number gutters left plain) and written file content |

## 5. Not recommended

- Porting to Rune today: pre-release, no PRs, weekly API churn, IME still the
  top open risk. See section 2 for the trigger to revisit.
- Multi-model fan-out (T3 shift-click models), guest share links, voice input,
  automations: valuable in those products but outside a thin opencode client's
  scope, or not supported by the opencode 2.x API.
- Floating "pet" status (Codex) and liquid-glass materials (Rune): style, not
  usability.
