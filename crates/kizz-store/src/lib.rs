// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The kizz cache: its location, its lock file, and the store of typed records.
//!
//! The cache is `KIZZ_CACHE_DIR` when set, otherwise `$XDG_CACHE_HOME/kizz`,
//! otherwise `~/.cache/kizz`. One build at a time holds its lock file, and the
//! records live in an embedded key-value database.
//!
//! Phase 0 of the implementation plan only creates this crate; phase 2 fills it.
