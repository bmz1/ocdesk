# Async state ownership: handoff

## Implemented

Branch: `refactor/async-state-ownership`, based on master `8ff3927`.
This is a bounded implementation of the engineering mission, ready for review;
it does not claim that every lifecycle in the application has been refactored.

- `domain/history.rs`: explicit Unloaded/Loading/Ready/Refreshing transitions;
  cached data survives refresh failure, and cancellation releases ownership.
- `domain/requests.rs`: distinct server-generation and connection-epoch types,
  explicit request scopes, and deterministic invalidation tests.
- Store-owned history tickets survive conversation deletion/eviction without
  reuse. Pure completion handling rejects stale, duplicate and foreign-session
  results before they can change messages/errors or emit StoreEvents.
- Controller disconnect/restart paths release abandoned history fetches and
  bounded replay buffers. Existing reconciliation and incremental events remain.
- `api/error.rs`: typed HTTP status through anyhow context; bounded diagnostics.
  Retry classification no longer depends on server/error text containing 404.
  Retry closures use concrete futures instead of per-attempt boxed futures.
- `controller/reads.rs`: receiver-owned cancellation for read-only requests.
  Mention/go-to-file search tasks now have explicit GPUI owners. Mutations retain
  their completion policy; cancellation never pretends to roll back server work.
- Structured debug traces identify history tickets and obsolete completions.
  AGENTS.md and the development guide document the implemented contracts.

## Continuation (branch `claude/beautiful-mccarthy-gttejj`, stacked on the above)

- `domain::requests::Revisions<K>` replaces four per-key counter maps (diff,
  tasks, permission mode, usage). Regression: per-key counters restarted at 0
  after `remove`, so a re-added folder's first diff/task request aliased a stale
  one. Folder removal now also invalidates its worktree checkouts' reads.
- `domain::choice::OptimisticChoices` orders overlapping model/agent choices.
  Regression: choosing A then B and A failing rolled the display back to the
  original over B. Agent switches (and `implement_plan`) now roll back on
  failure too; previously a failed agent switch left it displayed.
- Catalog, folder-info, working-diff and saved-grant reads use
  `run_on(RequestScope::Server)` instead of hand-compared generations.
- A usage response for a session deleted mid-flight no longer recreates its
  usage entry.
- Notice auto-clear, catalog debounce and settings-save debounce are owned
  GPUI tasks (replace = cancel) instead of detached tasks + sequence counters.
- `domain::fetch::FetchStatus(es)` replaces loading-bool/error-option pairs for
  saved grants, folder diffs and usage.
- Typed `GhFailed` for `gh` failures; "no pull request" is classified on its
  stderr, not on `e.to_string()`.
- Structured tracing fields for controller warnings/errors.

Continuation verification (this executor, Linux): `cargo fmt --all -- --check`,
strict Clippy, `cargo test --locked` (**158 passed**), the standalone domain
build (20 passed, no warnings), shellcheck, both live protocol suites against OpenCode 2.2.6,
and `dev/teardown-test.sh` under Xvfb with the mock model: all five exit paths
passed (sigterm, sigint, sigkill, panic, quit-after-idle). Desktop smoke checks
by hand (item 2 below) and macOS remain unverified.

## Continuation (branch `claude/zen-cori-feoxou`): approvals and supervision

- `store::Permissions` (`src/store/permissions.rs`) and `store::Forms`
  (`src/store/forms.rs`) own their requests, snapshot revisions, one
  `ReplyState` (Sending | Failed) per pending request and, for forms, the
  drafts. They replace store fields whose consistency relied on Store and
  Controller mutating them in step; collections are private. Store keeps
  notifications/StoreEvents, Controller keeps HTTP and connection policy.
  (An intermediate generic `domain::approvals` was replaced by these two
  concrete aggregates; no shared permission/form framework remains.)
- Regression: per-session `u64` revisions for permissions, forms and inboxes
  were removed with the session and restarted at zero. They are `Revisions`
  now, and `Revisions::freshness` tells Superseded (re-read) from Forgotten:
  a read overlapping session removal is dropped, success or failure, without
  re-reading or reinstalling the session's pending state.
- Regression: dismissing a form did not require a connection, so a dismissal
  sent while reconnecting was discarded on reconnect and the form stayed
  "sending" until the next disconnect. The Dismiss button is now disabled
  while disconnected, like Next and the permission buttons.
- `domain::supervisor::RestartBudget` is the sidecar crash-window policy, with
  time as an input. The delayed auto-restart is an owned `restart_timer` task
  (cleared by every start) instead of a detached timer re-checking the
  generation; giving up is logged at error level with the crash count.
- Tests: window-free aggregate tests (duplicate asks, session isolation,
  snapshot filtering/pruning, duplicate admission, stream closure before HTTP
  completion, late failure after removal, disconnect cleanup, draft lifetime),
  store tests for recursive child cleanup and unchanged StoreEvents,
  `Revisions::freshness` and restart-budget tests in the standalone domain build.

## Verification completed

Baseline: formatting, strict Clippy and all 125 existing Rust tests passed.
Final: formatting, strict Clippy, build and all **141 Rust tests** passed.
The additional tests cover real HTTP error classification, cancellation resource
release, completion ordering, cross-session isolation, delete/recreate, duplicate
callbacks, refresh failure and disconnect/restart. The pure domain tests also
pass standalone; their reference model checks 8,192 four-command sequences.

Executed:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
shellcheck packaging/linux/*.sh packaging/macos/*.sh dev/*.sh
python3 dev/m3-protocol-test.py /path/to/opencode2
python3 dev/desktop-features-test.py /path/to/opencode2
rustc --edition=2024 --test src/domain/mod.rs -o /tmp/ocdesk-domain-tests
/tmp/ocdesk-domain-tests
```

Both live protocol suites passed against OpenCode 2.2.6. Rendering/SSE hot paths
were not optimized; no measured latency, frame-time or memory benefit is claimed.

## Required before marking ready

1. Check Linux/macOS PR CI against current master. This executor only ran Linux.
2. Run desktop smoke checks: rapidly replace/close mention and go-to-file
   searches; select A then B during history loads; disconnect/reconnect during
   first load and refresh; verify cached rows, errors and fine-grained updates.
3. Run `dev/teardown-test.sh` with Xvfb and the mock model, preserving all five
   exit-path checks. It was **not run here**: this executor had no display, and
   an attempted local Xvfb startup failed to establish local/unix listening sockets
   (`Cannot establish any listening sockets`). No desktop screenshots were taken.
4. Coordinate with separate PR #24. It was not on the baseline master and these
   changes do not include its editor/config/explorer/plan fixes. When combining,
   update its newly introduced `issued(false)` plan continuation to
   `issued(RequestScope::Server)` and rerun the full checks. Do not merge stale
   combinations based only on independent branch test results.

## Further implementation deliberately left out

- Audit pending-state cleanup for other `run_on` callers. This change fixes
  history ownership, not every usage/grant/diff/form lifecycle.
- Review remaining detached work and PTY/sidecar startup lifetimes using concrete
  cancellation/teardown tests before changing process ownership or mutations.
- Consider additional pure execution/approval transition extractions only where
  they simplify the existing event fold. Do not add a universal reducer or traits
  for concrete structs simply to complete categories from the mission.
- Measure cold start, 500-message streaming frames and memory on a real desktop
  before making performance claims or changing transcript/rendering algorithms.

Read AGENTS.md and docs/development.md before continuing. The ripgrep comparison
used source revision `91f5d22ef48bde459d90a8c7dbeab416e97d0f2b`; adopted principles
are mechanism/policy separation, private invariants and behavior-based regressions,
not its multi-crate layout. No dependencies or pinned versions changed.
