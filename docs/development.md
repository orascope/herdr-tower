# Development

[← README](../README.md)

```bash
make build    # cargo build --release
make test     # cargo test --workspace  (the README badge has the count)
make lint     # cargo clippy --all-targets && cargo fmt --check
make install  # ~/.local/bin/herdr-tower
make smoke    # install, then ./TEST00.SH — make test plus the live scenarios
make hooks    # once per clone: every push must pass make test
```

**`make hooks`** turns on [`.githooks/pre-push`](../.githooks/pre-push). There is no
CI — GitHub Actions is blocked — so this is the gate: a push that touches
`rust/`, the `Makefile` or `TEST00.SH` runs `make test` first and is refused if
it fails. Tags, deletions and docs-only pushes skip it, and so does
`git push --no-verify`. It tests the checked-out tree, so it also refuses a push
of anything else, or one with uncommitted edits it would otherwise have tested
by mistake.

`make test` proves the code. **[`TEST00.SH`](../TEST00.SH)** runs it first and then
proves the product:
the dock coming back after its tab is closed or its tower is killed, the tab
keeping its name, the version reaching both the dock and the popup. It runs the
*installed* binary against a real herdr server — a disposable one it starts
itself, named `tower-smoke`, so the session you are working in is untouched.
`--live` points it at your own session instead, and `--only unit-tests` is just
`make test`, with no herdr involved.

It exists because every regression it guards against shipped with a green
`make test`. They were all in the seam between herdr-tower and a running
herdr: a stale token believed over a process listing, a version hidden on one
surface, a shell still running `/etc/zprofile` mistaken for a shell in use. No
unit test reaches those, because what was wrong was the conversation between
two live processes.

Rust 1.89 or newer is required, because of the standard library's file locks
(`File::try_lock`). Every crate in the workspace declares it as `rust-version`,
so cargo refuses an older toolchain before anything is built.

The CI workflow builds, tests and lints on **that declared 1.89 rather than the
newest stable** — a build that only works on latest makes `rust-version` a
decoration, and it twice caught a feature used below the stated minimum. It is
not running at the moment (see above), so nothing checks 1.89 automatically
today.

How the sixteen crates are arranged, and what each layer is responsible for, is
in **[`rust/README.md`](../rust/README.md)**. In short: `herdrx` is the herdr socket
client, `tower_state` the record store, `tower_scan` the pane-to-record reconciliation,
`tower_ui` the supervising TUI (ratatui), and `tower_cli` the commands and the
executable.

### What the tests hold

- **Against a real server.** `tests/live.rs` connects to a running herdr server
  and skips itself when there is none. A test that passes without the server
  proves nothing. Calls that move somebody's view (`focus`) and calls that poke an
  agent (`prompt`) are excluded from the tests.
- **Recorded output.** `status`, `info`, `help` and the card are rendered from
  a fixed store and a pinned clock and compared byte for byte with the
  expectations in **[`rust/expected/`](../rust/expected/README.md)**. Rewriting one
  is `UPDATE_EXPECTED=1 cargo test`, a deliberate act read in the diff.
- **Nobody's home directory is touched.** The `init` and `wt` tests all use
  injected paths and temporary folders.

## Not verified yet

Written down honestly. These need trying on real hardware.

- **The release pipeline.** There is no release workflow: GitHub Actions runs
  only the tests (`.github/workflows/ci.yml`). Every release is built on a Mac
  and uploaded by hand with `scripts/release.sh <vX.Y.Z> <out-dir>`: macOS
  natively, Windows with MinGW-w64, and Linux inside
  `quay.io/pypa/manylinux_2_28_{aarch64,x86_64}` containers
  (`scripts/build-linux.sh`, which refuses a binary asking for newer glibc than
  2.28). Before v0.23.1 Linux was cross-built with `cargo zigbuild` pinned to
  glibc 2.31, and those binaries did not start on RHEL 8 ("GLIBC_2.29 not
  found"). Before publishing, the macOS arm64 binary is run, and both Linux ones are run on RHEL 8.10 (UBI)
  and newer distributions (`docker run --platform linux/{amd64,arm64}`).
  `scripts/release.sh` does all of that but the newer distributions, then
  prints the tag and `gh release create` commands rather than running them.
  Each container is named for its role and version
  (`herdr-tower-build-<arch>-<version>`,
  `herdr-tower-verify-ubi8-linux-<arch>-<version>`) and runs with `--network
  host`. `HERDR_TOWER_BUILD_HOST_X86_64` / `_AARCH64` name an ssh host whose
  docker builds that arch, and `build-info.txt` in the output directory records
  the total build time and each step's.
  `scripts/remote-up.sh <vX.Y.Z> <release-dir> [ssh-target ...]` then installs the
  release on the machines `herdr machine list` knows (or the targets given):
  checksum, binary, `daemon install`, and a dock-tower restart.
  It explicitly sets and checks the macOS deployment target: 11.0 Big Sur
  for Apple Silicon (there is no Intel macOS build). This is the binary's declared
  minimum version; testing on the build Mac does not verify older macOS
  runtime behavior. Release notes distinguish those two kinds of evidence.

  Why the workflow does not cross-compile: `ring` — the TLS the HTTP client reaches through
  rustls — carries assembly, so building Linux from macOS needs a C cross
  toolchain. `zig cc` does it, but that is one more toolchain to install and keep
  working, and a native runner per target needs nothing.

  The Linux jobs sit on 22.04 for a reason too: a binary linked against a newer
  glibc does not start at all on an older distribution.

- **Compaction, codex and gemini.** claude's `session-start` has a `source ==
  "compact"` exception, so a mid-turn context compaction does not drop WORKING
  to IDLE. codex 0.156.0 sends SessionStart with `source` `compact` too (its
  sources, read from the binary: `startup`, `resume`, `clear`, `compact`,
  `fork`), so it has the same exception now. The stock Gemini CLI (0.46.0) never
  sends SessionStart for a compression — it has `PreCompress`, which is not
  registered — and agy registers no SessionStart. A live codex compaction was not
  reproduced: the run stopped at codex's hook review (below).
- **codex asks to trust its hooks again.** codex keeps a hash per trusted hook in
  `~/.codex/config.toml` (`[hooks.state]`); on 0.156.0 the herdr-tower hooks
  showed as "need review" although a hash was stored, and an untrusted hook does
  not run — the rows then have herdr's lifecycle alone. `/hooks` in codex, then
  `t`, trusts them; `herdr-tower doctor` shows a hooked kind that has never heard
  a hook.

- **The remote MCP server from a real client.** The whole OAuth and MCP flow is
  tested against the real binary on loopback, but it has not yet been connected
  from claude.ai or the phone app through a Tailscale Funnel. `send_prompt` has
  not been run against a real agent (it is herdr's `agent.prompt`, the call a
  broadcast makes), nor has `answer_prompt` pressed a key in a real dialog: the
  tests stop short of typing into somebody's session. What claude.ai
  sends on registration (its auth method, its callback, the protocol version) was
  written from the specifications, not observed.
