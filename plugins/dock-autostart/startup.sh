#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 ORASCOPE Co.,Ltd. (https://www.orascope.net)
set -eu

plugin_dir=$(CDPATH= cd "$(dirname "$0")" && pwd)
tower_bin=
if [ -r "$plugin_dir/herdr-tower-path" ]; then
    IFS= read -r tower_bin < "$plugin_dir/herdr-tower-path" || true
fi
if [ ! -x "$tower_bin" ] && command -v herdr-tower >/dev/null 2>&1; then
    tower_bin=$(command -v herdr-tower)
fi
if [ ! -x "$tower_bin" ]; then
    for candidate in \
        "${HOME:-}/.local/bin/herdr-tower" \
        /opt/homebrew/bin/herdr-tower \
        /usr/local/bin/herdr-tower
    do
        if [ -x "$candidate" ]; then
            tower_bin=$candidate
            break
        fi
    done
fi

if [ ! -x "$tower_bin" ]; then
    echo "herdr-tower executable not found" >&2
    exit 127
fi

# A newly restored Herdr server can report its first shell before that shell's
# foreground process has settled. `dock` deliberately refuses to type into a
# pane it cannot yet prove idle, so keep retrying in this background plugin
# command for up to five minutes. The command is idempotent and later attempts
# reuse the same dock workspace.
attempt=1
while [ "$attempt" -le 150 ]; do
    if "$tower_bin" dock --require-active; then
        exit 0
    fi
    if [ "$attempt" -lt 150 ]; then
        sleep 2
    fi
    attempt=$((attempt + 1))
done

echo "herdr-tower dock did not become ready after 5 minutes" >&2
exit 1
