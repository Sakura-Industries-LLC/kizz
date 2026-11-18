#!/bin/sh
# Copyright (c) 2026 Sakura Industries LLC.
#
# SPDX-License-Identifier: AGPL-3.0-only

# Build kizz with Cargo, then use it to install the tools pinned in mise.toml.
# Requires the Rust version in Cargo.toml, mise, Node.js and npm.
# Cargo resolves install dependencies independently of this checkout's Cargo.lock.
# Extra arguments go to cargo install, for example: ./bootstrap.sh --root "$HOME/.local"
set -eu
cd -- "$(dirname -- "$0")"

# Use an explicit installation root so the smoke test runs the binary we installed.
install_root=${CARGO_INSTALL_ROOT:-${CARGO_HOME:-"$HOME/.cargo"}}
has_root=false
next_is_root=false
for argument do
    if "$next_is_root"; then
        install_root=$argument
        next_is_root=false
        continue
    fi
    case "$argument" in
        --root) has_root=true; next_is_root=true ;;
        --root=*) has_root=true; install_root=${argument#--root=} ;;
        -h|--help|--list|-n|--dry-run) exec cargo install --path crates/kizz "$@" ;;
        --) break ;;
    esac
done
if ! "$has_root"; then
    set -- --root "$install_root" "$@"
fi
cargo install --path crates/kizz "$@"
exec "$install_root/bin/kizz" run install-tools
