// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The kizz build engine.
//!
//! This crate holds discovery, Markdown parsing, the Starlark runtime, step keys,
//! cache checks, explanations, and ordering. It contains no Linux-specific code: it
//! runs commands only through the tracer interface of `kizz_trace` and keeps its
//! records in `kizz_store`, so its tests use a fake tracer.
//!
//! Phase 0 of the implementation plan only creates this crate; later phases fill it.
