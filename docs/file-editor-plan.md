# Plan: file explorer + editor tabs (2026-11-08)

Goal: next to the chat, a **file viewer and basic read/write editor** in the
main area, as tabs: `[Chat] [main.rs ●] [Cargo.toml]`. Modeled on Orca
(stablyai/orca), adapted to ocdesk's constraints (GPUI, pure opencode client).
Nothing here is implemented yet.

## 1. Research

### 1.1 Orca (reference UX)

Sources: github.com/stablyai/orca README; onorca.dev docs pages
`model/tabs-panes-splits`, `editing/file-explorer`, `editing/monaco`,
`editing/viewers`, `review/diff-viewer`, `review/annotate-ai-diff`,
`agents/native-chat` (read 2026-11-08). Orca is Electron + Monaco.

| Area | What Orca does | Take for ocdesk? |
|---|---|---|
| Tabs | Every tab is one thing (agent chat, terminal, editor buffer, diff, PR, browser). Tabs live in groups inside panes, and panes split by dragging a tab to an edge. Each worktree owns its own tab layout. | **Yes, simplified**: one tab strip in the main area, with Chat pinned first. Splits come later (§5). |
| File tree | On the left of each worktree. Live on-disk updates, so agent edits "show up instantly". Git-status colors. Context menu: copy path, reveal, rename, stage/discard. | **Yes**: a **Files** tab in the right panel, with git colors from `/api/vcs/status`. |
| Opening | Click file paths in the transcript, or files in the tree or a turn's changed-file list. | **Yes**: tool cards, Changes panel, tree, and palette (`Mod+P`). |
| Editor | Monaco. Word wrap on, minimap off, Cmd-F find. | Use gpui-component's `Editor` (§1.3). |
| Save | **Autosave** on blur and idle, so no dirty dot. | **Not in v1**: explicit `Mod+S` with a dirty dot. Autosave becomes a later setting (§4.3). |
| Conflicts | Before each save, check the disk is unchanged since the last read or write. If it changed, autosave pauses and a banner offers *compare / reload / keep mine / overwrite*. | **Yes, the same flow** (§4.2). |
| Changes mode | An in-tab toggle between HEAD and the working tree. | Later (§5). The Changes panel already covers review. |
| "Add to chat" | Selected text → composer. | **Phase 2**: cheap and high value for an agent client. |

Not verifiable from Orca's docs: single-click vs preview tabs, and whether a
clean buffer reloads silently. We decide those ourselves (§4).

### 1.2 opencode 2.2.6 API (from `docs/openapi-opencode-2.2.6.json`)

Everything needed is on the server, so ocdesk stays a pure client:

| Need | Endpoint | Notes |
|---|---|---|
| List directory | `GET /api/fs/list?location[directory]=…&path=…` | Direct children `{data:[{path,type}]}`. Paths are relative to the location. |
| Read file | `GET /api/fs/read/<rel path>?location[directory]=…` | Raw bytes, with **ETag / Last-Modified**, `If-None-Match` → 304, and `Range` → 206. |
| Write file | `POST /api/experimental/fs/write?location[directory]=…&path=…` | Raw body → `{data:{path}}`. **Experimental**. No `If-Match`. **Not confined to the location**, so ocdesk must confine it itself (§4.4). |
| Watch file | `POST/DELETE /api/experimental/fs/watch` `{path}` | Watches **one file for 24 h**. Re-POSTing resets the expiry. Changes arrive on the SSE stream. |
| Change event | SSE `filesystem.changed` `{data:{file, event: add\|change\|unlink}}` | Not parsed yet: add it to `src/api/events.rs`. |
| Fuzzy find | `GET /api/fs/find` | Already wrapped (`Api::find_files`, used by `@` mentions). |
| Git status | `GET /api/vcs/status` | For tree colors. |

### 1.3 GPUI widgets available (gpui-kit 0.9.0 / gpui-component 0.9.0)

No new dependency is needed. Already in the tree:

- **`EditorState` / `Editor`** (`gpui_base::input::editor`): ropey-backed code
  editor with `.language("rust")`, line numbers, soft wrap, search panel
  (`searchable`), folding, indent guides, tab size, `set_readonly`, `set_value`,
  `InputEvent::Change`, decorations/diagnostics, and LSP hooks for later.
  Highlighting uses the **syntect** backend, already enabled via
  `gpui-kit/features=["syntect"]`. No tree-sitter features are needed.
- **`Tree` / `TreeState`** (`gpui_component::tree`): virtualized tree with a
  context-menu hook, used for the explorer.
- **`tab::TabBar` / `Tab`**: the tab strip.
- **`diff`** component: drives "Compare" in the conflict banner (the Changes
  panel already uses ocdesk's own `review.rs`; either works).
- `dock` (TabPanel/DockArea) exists but is a heavier layout system. Skip it
  for v1 (§2, decision 1).

The repo uses none of `EditorState`, `TreeState`, `fs/read`, `fs/list` or
`fs/write` today.

## 2. Decisions (recommendations)

1. **Tabs live in the main area, not the right panel.** `ConversationView`'s
   body becomes a `MainTabs` host. Tab 0 is **Chat** (the current
   conversation, can't be closed). Other tabs are file buffers. The right
   panel keeps Changes / Tasks / Terminal and gains **Files** (the tree), so
   you can browse with chat or a file visible. This is the "chat tab + editor
   tab" the request describes, and it leaves the conversation code alone.
   Not GPUI `dock`: too much layout machinery for one strip, and it fights
   the existing resizable panel.
2. **Open file tabs are scoped per folder/worktree directory**, like Orca's
   per-worktree layout, not per session. Switching sessions in the same
   folder keeps your files open. Switching to another folder swaps the strip.
   Persist `{dir → [paths], active}` in `settings.json`.
3. **I/O goes through the opencode API** behind a small `FileIo` trait in
   `src/api/fs.rs`, so remote locations and opencode's read confinement
   apply. A `tokio::fs` impl is the fallback if `experimental/fs/write`
   changes or disappears (it is experimental), and it serves as the unit-test
   double.
4. **Explicit save (`Mod+S`) plus a dirty dot in v1.** In an agent client the
   model edits the same files. Autosave would race the agent and silently
   overwrite its work. Autosave can come later as an opt-in setting.
5. **Clean buffers follow the disk. Dirty buffers never get clobbered.** See §4.2.

## 3. Architecture

```
src/api/fs.rs          FileIo trait + ApiFileIo (list/read/write/watch), ETag handling
src/api/events.rs      + Event::FileChanged { dir, file, kind }
src/store.rs           + files: HashMap<DirKey, OpenFiles>   (tabs, active, per-dir)
src/controller/files.rs open/close/save/reload, watch renewal, change routing
src/ui/main_tabs.rs    tab strip + body switch (Chat | Editor)
src/ui/editor_tab.rs   one buffer: EditorState + banner + status line
src/ui/files.rs        explorer (TreeState), lazy dir loading, git colors
src/settings.rs        + PanelTab::Files, + open_files per dir
```

**Buffer model** (`store.rs`):

```rust
pub struct Buffer {
    pub path: RelPath,               // relative to the folder
    pub etag: Option<String>,        // from last read/write
    pub disk_text: Arc<str>,         // what we believe is on disk
    pub editor: Entity<EditorState>, // UI-owned; text lives here
    pub dirty: bool,                 // editor text != disk_text (set on InputEvent::Change)
    pub conflict: Option<Conflict>,  // disk changed while dirty
    pub kind: BufferKind,            // Text | ReadOnlyLarge | Binary | Image
}
```

`dirty` comes from comparing against `disk_text`, debounced on `Change`, so
undoing back to the saved text clears the dot. Hashing works too if
comparing is too slow for large files.

## 4. Behavior spec

### 4.1 Open
- Sources: Files tree (click), tool cards (Read/Edited `filePath` becomes a
  link that opens at line `offset` if present), Changes panel ("Open" next to
  the external-editor button), palette **Go to file** (`Mod+P`, reuses
  `find_files`), and editor-tab links.
- An already-open file just focuses its tab. Single click opens and pins (no
  preview tabs in v1).
- Read with `Range: bytes=0-2097151`. **NUL in the first 8 KB** → Binary
  placeholder ("Open externally"). **Larger than 2 MB** (206 with
  Content-Range total) → read-only with a "large file" note. Images go
  through `ui/images.rs`.
- Language from the extension/filename, mapped to a syntect name (a small
  table plus a plain-text fallback).
- `POST fs/watch` on open, `DELETE` on close, re-POST every 12 h while open
  (the watch expires after 24 h).

### 4.2 External changes (agent edits)
Triggers: `filesystem.changed` for an open path, **and**
`session.tool.success` for Edit/Write tools on that path (belt and braces,
and instant). Then:
- **Clean buffer** → re-read with `If-None-Match`. On 200, `set_value` and
  restore the cursor and scroll (clamped). On 304, do nothing.
- **Dirty buffer** → no write. Set `conflict` and show a banner:
  *Changed on disk — Compare · Reload (discard mine) · Keep mine*. "Keep
  mine" keeps the dirty text and records the new etag as the base, so the
  next save is an explicit overwrite.
- `unlink` → banner "Deleted on disk". Save recreates the file.

### 4.3 Save (`Mod+S`)
1. `GET fs/read` with `If-None-Match: <etag>`.
   - 304 → write.
   - 200 → the disk moved: go to the conflict banner and don't write.
2. `POST experimental/fs/write`, then re-read headers for the new ETag (write
   returns none), and set `disk_text` and `dirty=false`.
3. Errors → notice bar (existing `controller.notice`). The buffer stays dirty.

The race between steps 1 and 2 is accepted. Closing a dirty tab, switching
folders with dirty tabs, or quitting (`on_app_quit`) → a confirm dialog:
*Save / Discard / Cancel*.

### 4.4 Safety
- Write paths are always `folder + RelPath`. Reject `..` and absolute paths
  client-side, because the server's write is not confined.
- Never write a buffer that was loaded truncated (large) or as binary.
- Keep the existing line endings (detect CRLF on load and restore it on save)
  and the trailing newline.

### 4.5 Keys (all unbound today; checked against `src/ui/keys.rs`)
| Keys | Action |
|---|---|
| `Mod+P` | Go to file |
| `Mod+S` | Save |
| `Mod+W` | Close tab (Chat can't be closed) |
| `Mod+Shift+E` | Right panel: Files |
| `Mod+1` | Chat tab · `Mod+2..9` file tabs |
| `Mod+Shift+]` / `[` | Next / previous tab (`Ctrl+Tab` stays session switching) |
| `Mod+F` | In an editor tab: the editor's own search (`searchable(true)`). In Chat: the existing find bar. |

Register them in `keys.rs` so the remapping and per-platform uniqueness test
cover them.

## 5. Phases

**Phase 1 — read-only viewer (≈1–2 days)**
- `api/fs.rs` list/read with tests against the mocked HTTP server pattern
  already used in `api/mod.rs` tests.
- `MainTabs` with Chat plus read-only `Editor` tabs. Open from tool cards
  and `Mod+P`.
- Files tree (lazy `fs/list` per expanded dir) as `PanelTab::Files`.

**Phase 2 — editing (≈2–3 days)**
- Dirty tracking, `Mod+S`, ETag precheck, write, confirm-on-close/quit.
- `filesystem.changed` parsing, watch lifecycle, clean reload, conflict
  banner with Compare (diff of `disk` vs `mine`).
- Persist open tabs per folder.
- "Add selection to chat" from the editor context menu: inserts a fenced
  snippet with `path:line` into the composer.

**Phase 3 — polish / Orca parity, optional**
- Git-status colors and context menu in the tree (copy path, reveal,
  open externally).
- Opt-in autosave (on blur and idle, still honoring the conflict check).
- Changes-mode toggle inside an editor tab (HEAD vs working tree).
- Split view: Chat | Editor side by side (a second body pane in `MainTabs`).
- LSP (`EditorState` already has hooks), only if it's ever wanted.

## 6. Verification
- Unit: path confinement, binary/large detection, CRLF round-trip, ETag
  flows (304/200), dirty after undo-to-saved, event parsing.
- `dev/` harness: open → agent edits the same file via the mock LLM → clean
  buffer reloads, dirty buffer shows the banner. Save → the file on disk
  matches.
- Demo mode: seed one open file tab for Xvfb screenshots.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## 7. Risks / open questions
- `experimental/fs/write` and `fs/watch` are explicitly unstable. The
  `FileIo` trait plus a local fallback limits the blast radius. Pin the
  behavior in `docs/protocol-notes.md` once probed against a real 2.2.6.
- Large-file performance of `EditorState` with syntect is unmeasured. Probe
  with a ~1 MB file before settling on the 2 MB cutoff.
- Does `filesystem.changed` fire for agent edits made through opencode's own
  tools (same process) as well as external ones? Probe it. The tool-success
  trigger in §4.2 covers it either way.
