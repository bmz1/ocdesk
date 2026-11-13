# Engineering boundaries

ocdesk is a pure OpenCode 2.x client. Keep the pinned Rust/GPUI/OpenCode versions
in sync with Cargo.toml, rust-toolchain.toml and CI; do not implement server agent
logic in the desktop. Read docs/development.md, docs/protocol-notes.md and
docs/gpui-notes.md before changing orchestration or rendering.

- controller/ and store/ are split by feature (see docs/development.md). Put new
  intents or state transitions in the matching submodule; keep controller.rs and
  store.rs to the shared struct, types and request helpers. Methods shared across
  submodules are pub(super); everything else stays private.
- Views send intents to Controller; HTTP and SSE stay in api/, off the GPUI
  thread. GPUI owns entity updates and fine-grained StoreEvent notifications.
- domain/ contains concrete, pure transition mechanisms. It must not import
  GPUI, touch disk/network, spawn tasks or read ambient time/settings.
- Store owns history tickets and cached conversations. Use begin_load and
  finish_load; do not install an async snapshot via reconcile directly. A
  refresh retains cached data; a stale/duplicate result must emit no effects.
- Per-resource latest-wins reads use domain::requests::Revisions (one
  counter for all keys; forget on removal). Do not reintroduce per-key
  counters that restart at zero: they alias stale responses after re-adding.
- Permission requests and question forms are owned by store::Permissions and
  store::Forms (private collections; Store emits events/notifications, the
  Controller owns HTTP and connection policy). One ReplyState per pending
  request, Sending or Failed, never both. Disconnect/restart abandon Sending
  only. A read whose session was removed meanwhile is Freshness::Forgotten:
  drop it and do not re-read. Never reply or approve on the user's behalf.
- Sidecar auto-restart policy is domain::supervisor::RestartBudget (pass the
  time in); the delayed restart is the owned restart_timer task.
- Optimistic model/agent changes go through domain::choice (show_model /
  show_agent, settle_*). Only the newest choice may roll back; never capture
  "previous" per request and restore it unconditionally.
- Debounce/timer work is an owned Option<Task<()>> (replace = cancel), not a
  detached task plus sequence counter. Controller::run is only for mutations
  whose result is persisted server state or local work; reads use run_on.
- Fetch status of cached data is domain::fetch::FetchStatus(es), never a
  loading bool plus error Option. Keep the cached value beside it.
- RequestScope::Connection fences stream-sensitive snapshots. RequestScope::Server
  allows an accepted mutation to finish across SSE reconnects on the same server.
  Per-resource revisions still protect newer state within either scope.
- run_on rejects obsolete callbacks; it does not undo HTTP effects or clean up
  their loading flags. Release owned pending state at the invalidation boundary.
- A stored GPUI Task is cancelled when dropped. Dropping a Tokio JoinHandle does
  NOT cancel it. The SSE pump explicitly aborts its Tokio task on stop/drop.
- controller::reads::spawn_read is only for read-only work. Its receiver owns
  demand; dropping it cancels the async future. Never use it for writes, prompts,
  approvals, terminal creation or process startup. Already-started blocking work
  can still finish; cancellation is not a rollback or synchronous cleanup barrier.
- Retain request/revision guards even for cancellable reads: cancellation and
  completion can race. Prefer typed HTTP errors over inspecting Display strings.
- The updater (api/update.rs, update.rs, controller/updates.rs) is the only code
  that contacts anything but the sidecar. It may check on a timer, but download,
  install and restart happen only on the user's explicit request. A download is
  used only after its SHA-256 matches the release manifest; replacements are
  staged beside the destination and renamed. Release asset names and
  SHA256SUMS.txt are its wire format: change them with domain/update.rs.
- Log operation, session/ticket, generation and epoch where useful. Do not log
  credentials, prompt/file content or authorization headers.

# Verification

For state/concurrency changes, add deterministic event-order/failure tests. Keep
captured-event and real OpenCode protocol checks; do not replace them with mocks.
Preserve row-level StoreEvents and transcript virtualization. The focused domain
models can also run without GPUI: `rustc --edition=2024 --test src/domain/mod.rs -o /tmp/ocdesk-domain-tests`.

Before committing, run `cargo fmt --all -- --check` and
`cargo clippy --locked --all-targets -- -D warnings`. Run `cargo test --locked`,
shellcheck on packaging/development scripts, and both dev/*protocol-test.py and
dev/desktop-features-test.py against pinned opencode2. See CI for build and
sidecar teardown checks. Record limitations; compilation is not a behavioral test.
