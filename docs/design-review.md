# Design and ergonomics review (2026-11-07)

A read-only review of the UI after M6, run against real opencode 2.2.6 with
the mock model (Linux/Xvfb). The status column records what was done about
each item in M7; the rest is backlog.

## Top priorities

| # | Finding | Proposed fix | Status |
|---|---|---|---|
| 1 | No macOS menu bar; default opaque title bar | `cx.set_menus` (App/File/Edit/View/Window); transparent title bar with traffic-light inset; header as drag region; reopen on Dock click | Menus done (M7); title bar backlog (needs a Mac to tune) |
| 2 | Permission card: equal-weight buttons, no shortcuts; shell requests show split resources, not the command; edit preview mostly diff headers and capped at 180 px; a `*` "always" rule is easy to miss | Show `$ command`; strip `Index`/`===`/`diff --git`/`index` lines; ~320 px cap; Allow once primary; warn-colored wildcard rule | Done (M7) except key shortcuts |
| 3 | Tool cards: every tool "Ran" with `>_`; description before command; raw JSON input; output looks like code blocks; trailing blank lines | Verb/icon per tool; command/file first; `$` line + terminal-style output; trim; JSON behind "Raw input"; group runs of tool calls | Partly (trim, per-tool verb); backlog |
| 4 | A turn reads as fragments: big gaps between steps, many "Thought ›", no turn footer | Tighter spacing within a turn; merge reasoning; hover footer with duration/model/tokens/Copy/Retry | Backlog |
| 5 | Composer draft shared across sessions; Enter dead while busy; composer blends into the page | Per-session drafts; Enter while busy steers / Ctrl+Enter queues; raised composer surface + focus ring | Done (M7) |

## Quick wins

- Sidebar: replace "! 1" with an icon + "Needs approval" tooltip; show both
  busy and waiting; unread dot for background turns. *(icon + tooltip done)*
- Sidebar row: overlay the hover ellipsis so relative times stay aligned. *(done)*
- Relative times only update on store changes: add a 30–60 s ticker. *(done)*
- Sidebar vs main background nearly identical: raise sidebar 3–4 % lightness.
- Changes view: center in the column; header-row Copy; strip git headers;
  line numbers and tinted +/− rows; collapsible files.
- Syntax highlighting (gpui-kit `syntect` / tree-sitter) and language labels.
- A single Esc stops a long turn: require Esc twice within ~1 s. *(done)*
- Keyboard: Cmd+1…9, Cmd+[ / ], F2 rename, Cmd+Backspace delete,
  Cmd+Shift+C copy last reply, Cmd+K switcher; grouped shortcuts sheet.
- Busy placeholder should say what Enter does. *(done)*
- Errors: Retry/Copy on turn errors; danger-tinted inline error card.
- Focus rings and tab order for rows, tool headers, approval buttons.
- Hit targets ≥ 24 px for hover icons; keep them visible on the selected row.

## Larger changes

- Sidebar filter (Cmd+F), pinned sessions, date separators, "Show more".
- Folder menu: Open in Finder/Terminal/Editor, Copy path.
- Arrow-key navigation of the session list.
- Multi-window; per-session scroll position; system notifications and Dock
  badge for finished/waiting background sessions.
- Perceived performance: skeleton loading, optimistic user bubble +
  "Thinking…" row, per-session `ProseCache`.

## Placement of new features (what M7 followed)

- **Subagents / background tasks:** inline row in the conversation that opens
  a right-side drawer with the child session read-only; running shells in the
  same drawer.
- **Queue / steer:** pending items as a compact stack above the composer;
  Enter steers while busy, Ctrl/Cmd+Enter queues.
- **`/` menu:** reuse the `@` popup; sections Commands, Skills, MCP; `/` only
  at the start of the prompt; compaction as a divider row in the transcript.
- **Git:** branch pill in the conversation header with a worktree menu;
  worktrees as sub-groups under their repo folder in the sidebar.
- Re-check typography on macOS (SF) before tuning sizes; body text may be
  larger than Codex's 14 px.
