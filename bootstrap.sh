#!/bin/sh
# Copyright (c) 2026 Sakura Industries LLC.
#
# SPDX-License-Identifier: AGPL-3.0-only

# Build and install kizz using Cargo; no kizz or documentation tools are needed.
# Requires the Rust version in Cargo.toml. Extra arguments go to cargo install,
# for example: ./bootstrap.sh --root "$HOME/.local"
set -eu
cd -- "$(dirname -- "$0")"
exec cargo install --path crates/kizz "$@"
