// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The store starts no thread: opening, using and closing it leaves the number of
//! threads of the process as it was, so it starts no `SurrealKV` clock thread and no
//! Tokio thread ("Kizz never panics").
//!
//! This is a test executable of its own, with one test, so no other test's threads
//! change the count while it runs.

#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "test code: the tests assert, lock and print"
)]

use std::{fs, io};

use kizz_store::{SCHEMA_VERSION, SchemaKey, SchemaVersion, Store};

/// The number of threads of this process, from `/proc/self/task`.
fn threads() -> io::Result<usize> {
    Ok(fs::read_dir("/proc/self/task")?.count())
}

/// The number of threads is the same before the store opens, after it opens, after a
/// transaction writes and another reads, and after it closes.
#[test]
fn the_store_starts_no_thread() {
    let dir = tempfile::tempdir().unwrap();
    let before = threads().unwrap();
    let store = Store::open(&dir.path().join("cache")).unwrap();
    assert_eq!(threads().unwrap(), before, "after opening the store");
    let mut txn = store.write().unwrap();
    txn.put(&SchemaKey, &SchemaVersion {
        version: SCHEMA_VERSION,
    })
    .unwrap();
    txn.commit().unwrap();
    let txn = store.read().unwrap();
    assert_eq!(
        txn.get::<SchemaVersion>(&SchemaKey).unwrap(),
        Some(SchemaVersion {
            version: SCHEMA_VERSION
        })
    );
    drop(txn);
    assert_eq!(threads().unwrap(), before, "after a write and a read");
    store.close().unwrap();
    assert_eq!(threads().unwrap(), before, "after closing the store");
}
