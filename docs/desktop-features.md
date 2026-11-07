# Attachments, permissions, settings, review and usage

Built on master `9a2b04f` (the M0–M7 implementation). This batch implements
requested items 4–8 and 10. Item 9 (commit, push, PR creation) is intentionally
excluded; existing branch/worktree controls are preserved.

## Attachments

Use the `+` button, drop regular files into the composer, or paste copied files
and images. Remove an attachment by clicking its chip. The prompt may contain
up to **100 files total**, including `@` file mentions. The composer and HTTP
client enforce this limit (opencode 2.2.6 itself does not enforce 100).
Attachment-only prompts work. Commands with attachments are sent as ordinary
prompts rather than silently discarding the attachments.

Pastes above 16,000 characters or 200 lines become a UTF-8 text attachment.
Clipboard images retain their encoded format. Pasted data is saved off the UI
thread under the platform data directory's `ocdesk/attachments/<random>/`.
On Unix these directories/files use modes 0700/0600. Files intentionally remain
available for queued prompts and later replay; no automatic cache eviction is
implemented. Remove this cache manually only after pending prompts have finished.

Draft attachments stay with their thread (and with the project for unsent new
threads). A paste finishing after a thread switch attaches to its original draft.
An in-progress paste blocks Send. Directories and missing files are rejected;
missing queued attachment paths prevent taking that queued prompt back for editing.
The transcript and pending queue show attachment names. The client retains
metadata, not base64 file bodies, in its conversation store.

## Permission modes

The composer changes the selected session's rules using `PATCH /api/session/id`.
A new thread receives its mode **before its first prompt**. Its composer choice
applies to that draft; defaults and project overrides are edited in Settings.
A server read confirms the displayed mode; unrelated rules display as Custom.

| Mode | Session rules |
| --- | --- |
| Supervised (default) | Ask for actions; allow read/glob/grep |
| Auto-accept edits | Supervised plus allow edit |
| Full access | Allow actions, subject to opencode's other restrictions |

**Probed on 2.2.6:** permissions are ordered arrays of
`{action, resource, effect}`, not v1 configuration maps. Saved project grants
precede the session's ask rules. Therefore Supervised can still allow actions
that have an existing saved grant. Settings displays this limitation and lists
saved grants with explicit **Revoke now** controls. Revoking affects every thread
in that project. Changing mode does not answer an already-pending approval or
cancel an executing tool.

## Notifications and unread

Background completion, failure, approval requests and forms mark their root
thread unread; subagent events route to the parent thread. A selected thread is
read only while the window is active. Activation/selection clears its unread
mark and retracts its notification where the OS supports it. State persists
across restarts. Removed threads have their unread mark removed.

GPUI's native notification API provides delivery and click-through to the
thread. Notifications default on and can be disabled independently of unread
indicators in Settings. macOS uses notification authorization; Linux requires a
session notification service. The window title shows the unread count, and
macOS also displays a Dock badge. Notification bodies contain only the thread
title, not prompts or tool output. Native macOS delivery/Dock behavior still
needs testing on a Mac; Linux headless testing cannot verify OS delivery.

## Settings

Open **Settings** in the sidebar or press Cmd/Ctrl+comma. Changes are staged until
Save settings (except explicitly immediate saved-grant revocations).

- Dark, light or system theme; theme changes apply immediately on save.
- Model (`provider/model`), agent and permission defaults for new threads.
  Project fields inherit global defaults when blank. Existing sessions keep
  their own choices. An unchanged model retains its saved reasoning variant.
- Editor executable, default `code`; arguments are not parsed as shell commands.
- Notification toggle and saved-project-grant management.
- App keybindings, with validation, duplicate rejection, and empty values to
  unbind. Saving rebuilds app bindings while preserving component/input bindings.
  Focused inputs keep their editing shortcuts when they conflict with app keys.

Existing settings files remain readable. Saves retain live window geometry,
unread state and viewed-file marks even if those changed while Settings was open.
`OCDESK_THEME` remains a startup theme override; a subsequent Settings save can
change the theme during the current run.

## Full diff review

Changes includes Latest turn, Working tree (HEAD → working copy), Whole branch
(base merge-base → working copy), Committed changes (base merge-base → HEAD), and
individual recent commits. Branch/committed scopes accept an explicit comparison
base, or use the repository default. Working/branch scopes include untracked
files. Errors remain visible with Refresh available; old requests cannot replace
newer scope results.

The server supplies VCS diffs. Individual-commit review uses read-only Git log
and show commands; merge commits show the first-parent comparison, root commits
are supported, and filenames are passed after `--`. No Git write actions were
added. The most recent 50 commits are listed.

Viewed marks persist by project/scope/base/file and the returned patch's
fingerprint: a different patch clears its mark. This tracks the returned patch,
not an independent content hash for binary or server-truncated files. Open in
editor resolves paths within the project and starts the configured executable
without a shell. Deleted/missing/outside-project paths report an error.
Patch previews retain the 500-line / 2,000-character-per-line display limit;
Copy patch includes the entire returned patch. Server truncation is labeled.

## Usage and context

The composer meter shows session tokens/cost, and a context estimate/bar when
available. Click it for input/output/reasoning, cache read/write, per-model
conversation usage and provider limits. Details update live and can be refreshed.

`session.usage.updated` replaces cumulative totals rather than adding them.
Session totals may include title-generation overhead; model rows aggregate
assistant messages only. Cache tokens are counted once, reasoning is reported
as a subset of output. Context is the latest assistant request's input plus
cache read/write from `/context`, compared with that model's context limit.
It is labeled an estimate, not an exact count of the next request. An implicit
server-default model is resolved from the context when absent from session info.
Provider limits from the experimental endpoint may be unavailable; that is
shown explicitly, including endpoint errors, rather than rendered as zero.

## Verification

Run the existing Rust checks plus:

```sh
python3 dev/desktop-features-test.py /path/to/opencode2
```

This uses a temporary Git repository, isolated settings/data, a real 2.2.6
sidecar and deterministic mock model. It tests encoded file URIs, attachment-only
prompts, PNG image transport, 100 files, mode changes, saved-grant precedence and
revocation, working/whole-branch/committed comparisons and usage/context/limits.
No provider credentials are required. CI runs this alongside existing tests.
