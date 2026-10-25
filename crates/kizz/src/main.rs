// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `kizz` command.

use std::process::ExitCode;

/// Parse the command line, run the selected command, and exit with its status.
fn main() -> ExitCode {
    kizz::cli::run()
}
