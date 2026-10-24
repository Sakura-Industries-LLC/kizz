// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The kizz tracer for Linux.
//!
//! It implements the tracer interface of `kizz_trace` with `ptrace` and a
//! seccomp-bpf filter that stops a traced process only for system calls that
//! touch the file system. Only the `kizz` binary wires it in.
//!
//! Phase 0 of the implementation plan only creates this crate; phase 1 fills it.
