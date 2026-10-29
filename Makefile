.PHONY: build test smoke lint install hooks clean

build:
	cd rust && cargo build --release

test:
	cd rust && cargo test --workspace

lint:
	cd rust && cargo clippy --workspace --all-targets && cargo fmt --check

# The whole gate: the unit suite, then the live scenarios against a real herdr
# server — the dock coming back, the version reaching the surfaces people read.
# Every regression the live half guards against shipped with a green
# `make test`, which is why it exists.
#
# 🚨 `test` before `install`, not after. The live half tests the *installed*
# binary, so it has to install first — and installing replaces the binary every
# pane runs and restarts the supervisor. A tree whose unit tests fail must stop
# before that, not be reported once the damage is done. TEST00.SH is told not
# to run the suite a second time.
smoke:
	$(MAKE) test
	$(MAKE) install
	./TEST00.SH --no-unit

# Local install — the hooks have to find herdr-tower on PATH.
# The hook commands that get registered write this path out literally, so
# moving the binary means running `herdr-tower init` again.
#
# Removed before it is replaced, rather than written over. macOS binds the
# ad-hoc signature to the inode, and overwriting one that a running process has
# mapped invalidates it — the tower and every hook already up are killed
# (SIGKILL, exit 137) the moment they next fault a page in. A fresh inode
# leaves them running on the old one until they exit by themselves.
install: build
	install -d $(HOME)/.local/bin
	rm -f $(HOME)/.local/bin/herdr-tower
	install -m 0755 rust/target/release/herdr-tower $(HOME)/.local/bin/herdr-tower
	$(HOME)/.local/bin/herdr-tower daemon install

# Turns on .githooks/pre-push, which refuses a push whose code fails `make test`.
# Once per clone. It is set on the repository, so every worktree of it — the
# ones /gnew makes and the ones agents work in — uses the same gate.
#
# 🚨 An absolute path to the main checkout's copy, not `.githooks`. A relative
# hooksPath resolves in whichever worktree is pushing, and a worktree on a
# branch older than the hook has no `.githooks` — so its pushes would skip the
# gate without a word.
hooks:
	git config core.hooksPath "$$(dirname "$$(git rev-parse --path-format=absolute --git-common-dir)")/.githooks"
	@echo "pre-push gate on: $$(git config --get core.hooksPath)"

clean:
	cd rust && cargo clean
