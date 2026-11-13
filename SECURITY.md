# Security policy

Please **do not** report security problems in public issues.

Report them privately through GitHub's
[private vulnerability reporting](https://github.com/bmz1/ocdesk/security/advisories/new).
Include steps to reproduce and the ocdesk and opencode versions. You should
get a response within a week.

Things that are in scope include the sidecar's local HTTP auth (random
per-launch password, loopback only), file reads and writes through the editor
tabs (path confinement to the folder), the updater (HTTPS-only download from
this repository's releases, verified against the release's SHA-256 list before
anything is replaced) and the packaging scripts. Problems in
opencode itself should go to the [opencode project](https://opencode.ai).
