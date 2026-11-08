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

- Order overlapping optimistic model/agent mutations and reconcile failures;
  inspect `Controller::choose_model` and `choose_agent` with HTTP/SSE interleavings.
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
