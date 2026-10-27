// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The `kizz` command-line tool.
//!
//! The `kizz` binary is a thin wrapper over this library's [`cli`] entry point,
//! which parses the command line and prints through the terminal helpers of the
//! `ui` module. The binary is the only crate that wires the engine of `kizz_core`
//! to the Linux tracer of `kizz_trace_linux`.

mod build;
mod cache;
pub mod cli;
mod execute;
mod list;
mod ui;
mod why;
