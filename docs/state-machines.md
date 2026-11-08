# State machines and transition ownership

This is a source-level inventory of ocdesk's application lifecycles, including
machines expressed through flags, collection membership, optional values and
async continuations. It covers production Rust under `src/`; server-owned agent
execution is observed, not reimplemented. A row can describe a family of keyed
machines (one per session, folder, file or request). This is not a proof that all
possible event interleavings are safe.

## Decision

Keep independent state independent. A session can execute while its history is
loading and while a permission request is waiting: those are separate machines,
not mutually exclusive session statuses. Use enums where states really exclude
one another, retain revisions/tickets for stale async results, and test ordered
event sequences outside GPUI wherever possible. No state-machine framework or
protocol/persistence migration is needed.

UX means preserving drafts, accurate progress and predictable targeting. DX means
one owner for each transition and reviewable failure handling. AX includes both
coding agents understanding these contracts and the running OpenCode agent
sharing the checkout without losing or silently overwriting changes.

## Inventory

“Explicit” means an enum already names the states. “Implicit” means callers must
interpret several fields or a continuation. “Mixed” combines both. Source links
identify the owner rather than a second parallel implementation.

| Machine / key | Representation and transitions | Owner / invariant / decision |
| --- | --- | --- |
| Sidecar lifecycle / process generation | Mixed: Starting → Connecting → Connected; unexpected exit → SidecarDown → delayed restart, with crash-window budget | [controller](../src/controller.rs), [sidecar](../src/sidecar.rs), `Connection` in [store](../src/store.rs). Keep generation guards, explicit pump abortion and process-group teardown. |
| SSE transport / connection | Mixed: connect → frames → timeout/EOF/error → Disconnected → backoff → reconnect | [API](../src/api/mod.rs), controller event pump. Keep separate from process liveness; a dropped SSE connection does not mean execution stopped. |
| SSE frame parser | Implicit: incomplete line → complete data lines → blank-line frame; bounded buffering | `SseParser` in API. Preserve framing tests and size limits. |
| Request validity / callback | Implicit: issued generation + optional connection epoch → accepted or discarded | `Issued`, `run_on`, `is_current` in controller. A continuation must not run against a replacement server. |
| Snapshot reconciliation / resource | Implicit: snapshot request → revision comparison → bounded delayed retry → merge | Controller session/active/inbox/tasks/approvals reads; store revisions. Keep server snapshots and live events coordinated. |
| Conversation history / session | Implicit: unloaded/loaded × fetching; load ticket, live ids, buffered events, load error | `Conversation`, `begin_load`, `finish_load`, `reconcile_inner` in store. Latest fetch wins; local streamed extensions survive. Keep fixture replay coverage. |
| Transcript cache / session | Implicit: selected/recent/live → retained; inactive and over budget → evicted | Store `recent`, `evict_conversations`, `invalidate_except`. Retention is independent of history-fetch status. |
| Prompt admission / submission id | Implicit: draft/session target → creating/configuring session → HTTP admission → accepted or uncertain | `submit_to`, `submitting`, `SubmissionTarget`. HTTP acceptance is separate from execution completion. Preserve explicit recovery; do not automatically resend uncertain admissions. |
| Admission recovery / last failed request | Implicit: absent → recovery tuple → explicit retry; can be replaced by another failure | Controller `failed_submission`. Original session/folder must travel with recovery. |
| Plan implementation / original session | Implicit async chain: switch agent → submit implementation prompt, or error | **Changed:** `implement_plan` captures session/folder, guards its continuation by server generation (`RequestScope::Server`), shows Build optimistically through `domain::choice` (rolled back if the switch fails) and changes the draft default only after success. Changing selection while awaiting must not redirect the prompt. |
| Execution / session | Implicit: busy membership, retry text, interrupt-in-flight, error; execution events enter/leave busy | Store `set_busy_inner`, `set_active`; controller abort. Keep separate from submitting, approvals and reconnecting. |
| Assistant step / message | Mixed: started → blocks/deltas/tools → finished/failed, completion/error metadata | Store event fold, [model](../src/api/model.rs). Preserve idempotent terminal handling and transcript fixtures. |
| Tool call / call id | Explicit `ToolStatus`: pending/running → completed or failed; input/output can arrive separately | Model and store. Terminal tool status is server-owned; unknown tools may modify files. |
| Inbox delivery / item id | Explicit `Delivery` plus implicit pending/edited/promoted/deleted/delivered membership | [composer](../src/ui/composer.rs), controller inbox operations, store. Pending text is not transcript history until delivered. |
| Approval / (session, request) | Implicit: pending → sending → replied/removed, or visible retryable error; reconnect re-fetches | Store permissions/revisions/sending/errors; controller reply; [permission UI](../src/ui/permission.rs). Keep server authoritative; never auto-approve on recovery. |
| Question / (session, form) | Implicit: defaulted/editing draft → reply or cancel in flight → removed or retryable error | Store forms/drafts/revisions; controller; [form UI](../src/ui/form_view.rs). Draft lifetime and request lifetime are distinct. |
| Agent/model choice / session or draft | **Explicit now:** `OptimisticChoices` per session: choose → newest outstanding token; success/failure/stream report → settle or roll back to last server-known value | [domain::choice](../src/domain/choice.rs); controller `show_*`/`settle_*`. Only the newest choice may roll back; the stream stays authoritative. Do not collapse draft and active-session choices. |
| Permission mode / session or draft | Implicit: inherited/known → mutation in flight → confirmed or error, versioned reads | [feature controller](../src/controller/features.rs), store mode maps. A read predating a mutation must not overwrite it. |
| Saved grants / grant id | `FetchStatus` (Idle/Loading/Failed) beside the cached list; individual revoke in flight → refreshed list | Feature controller, store saved-grant state, [settings](../src/ui/settings.rs). Keep server persistence authoritative. |
| Usage / session | Implicit: cached summary/details → loading; details requested during summary → deferred detailed read; live totals can supersede snapshot | Feature controller, store usage maps/revisions, [usage UI](../src/ui/usage.rs). Summary refresh must retain detail fields. |
| Diff snapshot / folder | Implicit: absent/cached/stale → loading → ready/error; request sequence rejects old response | Controller diff functions, store diff maps, [review](../src/ui/review.rs). Cached data and freshness are independent. |
| PR polling / checkout | Implicit: never fetched/no PR/PR/error × loading; window activity and pending checks select poll interval | [GitHub controller](../src/controller/github.rs), store `PrEntry`. Keep cached PR visible during fetch. |
| Check log / check URL | Explicit `CheckLog`: Loading → Ready or Failed; new push invalidates old logs | GitHub controller, store, [PR UI](../src/ui/pull_request.rs). Preserve commit-sensitive invalidation. |
| Unread attention / root session | Implicit: background event → deduplicated notification + unread; focused selection → read + badge update | [notifications](../src/notifications.rs), feature controller, settings unread. Child activity routes to parent attention. |
| Settings persistence / save sequence | Implicit: changed → debounce → snapshot → serialized atomic write; stale snapshots discarded | Controller save functions, `PendingSave` in [settings](../src/settings.rs). Preserve ordered-write tests and atomic rename. |
| Quit / window | Implicit: close request → dirty-file/config confirmation → cancel or quit + sidecar teardown | [app view](../src/ui/app_view.rs), main and sidecar guards. Save-in-flight is meaningful even when current text is clean. |
| Composer draft / session or folder draft | Implicit: selected scope → stash previous text/attachments → restore new scope → submit/clear | Composer `draft_scope`, `swap_draft`. Preserve per-folder new-session drafts. |
| Attachment preparation / captured draft scope | Implicit: picked/dropped/pasted → validation or background temporary write → attach or error | [attachments](../src/ui/composer/attachments.rs). Async completion attaches to its captured scope, not current selection. |
| Mention and slash search / query generation | Mixed: closed → popup/query → searching → suggestions → accept/dismiss; sequence rejects old query | Composer `MentionPopup`, `mention_seq`; [mention](../src/ui/mention.rs). A stale result must not reopen a dismissed popup. |
| Prompt history / draft | Implicit: live draft ↔ recalled index; changing recalled text exits recall | `History` and composer recall helpers. Preserve original draft when navigating back to newest. |
| Keyboard stop / composer | Implicit: unarmed → first Escape timestamp → second Escape within window → abort; typing/disarm reset | Composer `esc_armed`, `escape_to_stop`. Keep separate from permission keyboard responses. |
| Configuration operation / editor | **Explicit now:** Idle ↔ Loading / Saving / Reloading; dirty baseline, inherited scope and errors are independent | [config editor](../src/ui/settings/config.rs). One operation at a time; scope changes pass through the same guard; loading and saving have distinct progress text. |
| MCP settings / project | Implicit: unloaded/loaded × refresh/connect/disconnect in flight → result/error | [MCP settings](../src/ui/settings/mcp.rs). Project choice is locked during requests; preserve last visible server list on failure. |
| Agent-document editor / path | Implicit: discover → open/edit/stash → pending save/delete/create → completion/conflict/error | [agents UI](../src/ui/agents.rs), [agent files](../src/agents.rs). Path identity and edit/reload sequences protect drafts across navigation. |
| File content / buffer id | Mixed: Loading → Ready/Binary/Failed; dirty baseline, truncation, disk conflict/deletion are independent | [editor tabs](../src/ui/editor_tabs.rs). Keep explicit content status and conflict enum; preserve CRLF and read-only truncated files. |
| File I/O / buffer id | **Explicit now:** Idle → Running(Load/Revalidate/Save) → Idle; invalidations set one pending follow-up read | `BufferIo` in [lifecycle](../src/ui/lifecycle.rs). Reject overlapping loads/saves; release on success and failure; retain edits typed during save. |
| File conflict resolution / buffer id | Explicit conflict plus implicit user intent: changed/deleted → reload disk or keep mine; save rechecks disk/readback | Editor tabs. **Changed:** conflict actions cannot mutate the baseline while I/O owns it. Keep pre-write and post-write conflict detection. |
| File close / buffer id | Implicit: clean → remove; dirty → Save/Don't Save/Cancel; close-after-save only after clean success | Editor tabs. Keep close intent independent of I/O and current text; a new edit during save prevents automatic close. |
| Explorer invalidation / folder set | **Explicit now:** None → Folders(union) or All; latest debounce timer drains accumulated targets | [explorer](../src/ui/explorer.rs), `RefreshTargets`. Folder B must not erase folder A or narrow a global invalidation. |
| Explorer directory read / (folder, path) | **Explicit now:** absent → Loading → RefreshPending on another invalidation → one follow-up read | `DirectoryReads` in lifecycle; explorer owns the resulting I/O. Fresh invalidations during a listing are retained instead of dropped. Expansion/listing cache remains independent. |
| Terminal attachment / session | Mixed explicit Connecting/Restarting/Live/Exited/Failed plus optional sender/task | [terminal](../src/ui/terminal.rs), [PTY API](../src/api/pty.rs). Restart destroys old PTYs; reconnect preserves server PTY; LRU detaches only local emulator. |
| Background task inspection / drawer target | Mixed `TaskView` + optional poll task/output; target change cancels old polling; results keyed to target | [tasks](../src/ui/tasks.rs). Keep shell output and subagent transcript lifetimes distinct. |
| Review comments / file+line | Implicit: no editor → draft → committed local comments → compose one agent prompt or discard | Review and [changes](../src/ui/changes.rs). Content-hashed viewed marks track reviewed versions, not just file paths. |
| Navigation/search/focus / view | Mixed optional right-panel tab, active editor index/Chat, selected session, `FindState`, palette `Mode` | Conversation, app view, [palette](../src/ui/palette.rs). Independent regions can remain simple; persist semantic preferences, not transient focus. |
| Folder registration / path | Implicit: native picker or typed path → location registration → settings/store insertion → draft selection; removal hides sessions without deleting server data | Controller `add_folder`, `remove_folder`; [UI dialogs](../src/ui/mod.rs). Async location completion must retain its captured folder. |
| Worktree/session-location mutation / checkout | Implicit: requested → create/remove/move in flight → refresh/select; dirty removal → explicit force confirmation or cancel | Controller worktree functions, [git dialogs](../src/ui/git.rs). Session location, current folder and selected session are related but distinct. |
| Session metadata mutation / session | Implicit: rename/archive/restore request → server event or refresh/error; pin is local persisted membership | Controller session functions, feature controller, [sidebar](../src/ui/sidebar.rs). Archiving selected session leaves selection first; hidden is not deleted. |
| Catalog refresh / folder | Implicit: burst event → debounce → parallel model/agent/tooling/VCS/project reads → partial result | Controller catalog functions; store `Catalog`, `Tooling`. The debounce is an owned GPUI task (replace = cancel); reads complete via `run_on`. Independent reads may fail separately. |
| Notice lifetime / notice timer | Owned task: notice → timer task stored on the controller; a newer notice replaces (cancels) it; user dismissal clears the current notice | Controller `notice`, `notice_clear`; conversation banner. An older timer cannot dismiss a newer failure. |
| Go-to-file search / dialog query | Implicit: query → debounce → request → accepted only for latest sequence → selected result → close/open | [go-to-file](../src/ui/goto_file.rs). The dialog captures a directory for search; opening resolves through current tabs, so selection changes while the dialog is open deserve a targeting check. |
| Preferences draft / settings view | Implicit: snapshot → edit locally → validate keybindings → save selected fields or close/discard | Settings view, controller `save_preferences`. Preserve live window, unread and reviewed-file fields when saving the screen's draft. |
| File watches / open tab | Implicit: open → subscribe → renew every 12 h → unsubscribe on close; reconnect triggers full revalidation | Editor tabs, [file controller](../src/controller/files.rs). Watch delivery is advisory; activation and save still validate disk state. |
| Presentation caches / revision or block key | Implicit: missing → derived → invalidated/evicted; expanded tool/prose state is view-owned | [prose](../src/ui/prose.rs), conversation todos memo, [LRU](../src/ui/lru.rs), syntax/diff caches. Avoid turning derived render data into a competing app-state owner. |

## Concrete changes and regression sequences

- Folder A invalidated, then B before debounce: both are refreshed. Global then
  folder-specific invalidation: all folders remain scheduled.
- A directory refresh arrives while its listing is in flight: one further read
  runs after completion, including after failure.
- Load/save/revalidate starts, then many file notifications arrive: no overlapping
  operation, one follow-up read, then idle if no more notifications arrive.
- Conflict resolution during a read/save: defer the action rather than replacing
  the baseline under the operation.
- Config load/save/reload: one named operation, one terminal reset; dirty and
  inherited/read-only remain separate facts. Scope callbacks use `show_project`.
- Implement plan in A, switch selection to B while the build-agent request runs:
  the prompt still targets A. A sidecar generation change discards the continuation.
  A failed switch rolls the optimistic Build selection back.

`src/ui/lifecycle.rs` contains window-free event-sequence tests. Existing store
fixture replay, file save/readback, settings ordering and parser tests remain the
regression coverage for their respective machines. The plan/navigation scenario
also needs an interactive check; a source audit is not UI validation.

For a fast transition check without building GPUI, run from the repo root:

```sh
rustc --edition=2024 --test src/ui/lifecycle.rs -o /tmp/ocdesk-lifecycle-tests
/tmp/ocdesk-lifecycle-tests
```

These same tests run as part of `cargo test --locked`. The standalone check does
not replace compilation, store fixture tests or live protocol/UI checks.

## Follow-up boundaries

These are concrete audit risks, not claims of reproduced production failures:

1. **Resolved (#26):** overlapping model/agent choices are ordered per session by
   `domain::choice`; stream reports and accepted older choices update the rollback
   target. An older success arriving after the newest choice settled still relies
   on the stream echo to correct the display.
2. `run_on` intentionally drops stale callbacks. Any loading/sending flag whose
   only cleanup is in that callback needs owner-level reset or cancellation
   cleanup. Sidecar restart clears many flags already; history load tickets are
   owned by the store (#25). PR/check-log loading lifetimes deserve separate
   cancellation tests.
3. **Audited (#26):** reads and optimistic choices use `run_on`. Remaining `run`
   callers are mutations whose result is persisted server state, or local work;
   `Controller::run` documents that policy. Original-selection ownership of fork,
   move and worktree completions still deserves interactive checks.
4. Config reload checks that sessions are idle before issuing its request; it
   does not reserve an app-wide admission lock until completion. Preventing a
   concurrent new prompt needs a controller-owned reload/admission contract.
5. File save performs preflight validation and readback, but the server write API
   is not an atomic compare-and-swap. It detects many races; it cannot guarantee
   another writer's changes are never overwritten in the check-to-write gap.

These boundaries are why the change keeps protocol, on-disk schemas, permission
semantics and the existing reconciliation model intact instead of attempting a
single global refactor. Require regression evidence before migrating a family.
