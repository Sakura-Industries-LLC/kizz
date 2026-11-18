#!/bin/sh
# Copyright (c) 2026 Sakura Industries LLC.
#
# SPDX-License-Identifier: AGPL-3.0-only

# Build and install kizz using Cargo; no kizz or documentation tools are needed.
# Cargo install without --locked resolves dependencies independently of this
# checkout's Cargo.lock; it may use newer dependencies than the quality gate tested.
# Requires the Rust version in Cargo.toml. Extra arguments go to cargo install,
# for example: ./bootstrap.sh --root "$HOME/.local"
set -eu
cd -- "$(dirname -- "$0")"
exec cargo install --path crates/kizz "$@"
