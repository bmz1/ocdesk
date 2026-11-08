# Contributing to ocdesk

Thanks for your interest! Bug reports, fixes and small focused features are
all welcome.

## Before you start

- For anything bigger than a bug fix, open an issue first so we can agree on
  the approach. ocdesk is deliberately a *pure client*: agent logic, tools,
  providers and permissions belong in opencode, not here.
- ocdesk targets one pinned opencode version (see *Pinned versions* in the
  [README](README.md)). Protocol changes need updated fixtures and
  [docs/protocol-notes.md](docs/protocol-notes.md).

## Setup

Install the Rust toolchain from `rust-toolchain.toml` (rustup does this
automatically), the Linux build dependencies listed in the README, and
`@opencode/cli` at the pinned version. You don't need an API key:
`dev/mock_llm.py` is a deterministic offline model, and `OCDESK_DEMO=<turns>`
runs the UI with no opencode at all. See *Developing* in the README.

## Checks

CI runs all of these; please run them before opening a PR:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
shellcheck packaging/linux/*.sh packaging/macos/*.sh dev/*.sh
python3 dev/m3-protocol-test.py "$(command -v opencode2)"   # needs opencode 2.x
```

## Pull requests

- Keep PRs focused; one change per PR.
- Add or update unit tests for store/protocol changes. Prefer replaying real
  captured events (`tests/fixtures/`) over hand-written ones.
- For UI changes, include a screenshot or short recording.
- Update the README when you change behavior, shortcuts or env vars.

By contributing you agree that your contributions are licensed under the
[MIT License](LICENSE).
