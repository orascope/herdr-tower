## What and why

<!-- What this changes, and the problem it solves. Link the issue if there is one. -->

## Checked

- [ ] `make test` passes (or `cargo test --workspace` in `rust/`)
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check` are clean
- [ ] Behavior changes come with a test; output changes update `rust/expected/` on purpose (`UPDATE_EXPECTED=1`)
- [ ] No real machine names, user names, paths or task titles in fixtures, docs or screenshots
