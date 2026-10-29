# Contributing to herdr-tower

Issues and pull requests are welcome. herdr-tower is maintained by
ORASCOPE Co.,Ltd.; a maintainer reviews and merges every change.

## Before you start

- **A bug?** Open an issue with the bug template: `herdr-tower version`,
  `herdr --version`, the OS, and what you saw.
- **A feature?** Open an issue first and describe the problem. A change that
  adds a key, a setting or a command is easier to agree on before it is written.
- **A security problem?** Do not open a public issue; see [SECURITY.md](SECURITY.md).

## Building and testing

The code is a Rust workspace under `rust/` (Rust 1.89 or newer).
[docs/development.md](docs/development.md) describes the crates, the test
layers and what is still unverified.

```bash
make build    # cargo build --release
make test     # cargo test --workspace
make hooks    # once per clone: every push must pass make test
```

Tests that need a running herdr server skip themselves without one, so
`make test` works anywhere. CI runs the tests, `clippy -D warnings` and
`cargo fmt --check` on Linux (amd64, arm64), macOS and Windows for every pull
request; a first-time contributor's run starts once a maintainer approves it.

## Pull requests

- One change per pull request, with a test for any change in behavior.
- Output the tower prints is recorded in `rust/expected/`; rewrite it with
  `UPDATE_EXPECTED=1 cargo test` only when the change is meant, and say so.
- Every source file starts with the SPDX header
  (`SPDX-License-Identifier: Apache-2.0` and the copyright line); a test
  checks it.
- Fixtures, docs and screenshots use invented names: no real machine names,
  user names, paths or task titles.
- Commit messages, comments and docs are in English.

## License

herdr-tower is licensed under the [Apache License 2.0](LICENSE). Unless you
say otherwise, a contribution you submit is licensed under the same terms
(section 5 of the license).
