// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The record store in temporary cache directories: persistence across closing,
//! reopening, and crashes; transactions; prefix scans; the schema version; and
//! sharing between threads.
//!
//! The crash test runs this test executable again in a helper mode:
//! [`helper_process`] does nothing in a normal test run, and writes records into the
//! cache that its explicit environment names when started with the helper variable
//! set. It reports on its standard output, in lines that start with [`MARK`].

use std::{
    io::{BufRead as _, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
};

use kizz_store::{
    AccessBits, BuildId, Capture, ContentHash, DATABASE_DIR, EntryCounts, FileMeta, Inventory,
    LastWriter, LearnedOrder, Observation, Owner, PathKey, PathState, PositionKey, ProgramPath,
    RecordedOutput, RunReason, SCHEMA_VERSION, SchemaKey, SchemaVersion, StepExit, StepKey,
    StepPosition, StepRecord, StepSpecRecord, Store, StoreError, TaskId, TaskRecord, Times,
    Verdict, WrittenBy, path_prefix,
};

/// The variable that selects the helper mode of this test executable.
const HELPER_VARIABLE: &str = "KIZZ_STORE_TEST_HELPER";
/// The variable that names the cache directory of a helper.
const CACHE_VARIABLE: &str = "KIZZ_STORE_TEST_CACHE";
/// The start of every line a helper reports.
const MARK: &str = "kizz-store-helper\t";

/// A step record whose fields all derive from `n`, owned by two tasks.
fn step_record(n: u8) -> StepRecord {
    StepRecord {
        spec: StepSpecRecord {
            program_hash: ContentHash([n; 32]),
            cwd: PathBuf::from(format!("/w/{n}")),
            argv: vec!["tool".to_owned(), n.to_string()],
            env: [("N".to_owned(), n.to_string())].into(),
            stdin: None,
            capture: Capture::Split,
            stream: false,
            always: false,
            ignore: Vec::new(),
            track: Vec::new(),
            timestamps: n % 2 == 1,
            expect: None,
            occurrence: u32::from(n),
        },
        program: Some(ProgramPath(PathBuf::from("/bin/tool"))),
        observations: vec![
            Observation {
                path: PathBuf::from(format!("/w/{n}/input")),
                access: AccessBits(AccessBits::READ),
                tracked: false,
                state: PathState::File {
                    meta: FileMeta {
                        size: u64::from(n),
                        mtime_nanos: i64::from(n),
                        ctime_nanos: i64::from(n),
                        inode: u64::from(n),
                        device: 1,
                        mode: 0o100_644,
                        owner: Owner {
                            uid: u32::from(n),
                            gid: 100,
                        },
                    },
                    hash: Some(ContentHash([n; 32])),
                },
            },
            Observation {
                path: PathBuf::from(format!("/w/{n}")),
                access: AccessBits(AccessBits::PROBE),
                tracked: false,
                state: PathState::Directory {
                    mode: 0o040_755,
                    owner: Owner {
                        uid: u32::from(n),
                        gid: 100,
                    },
                    listing: None,
                    times: Times {
                        mtime_nanos: i64::from(n),
                        ctime_nanos: -1,
                    },
                },
            },
        ],
        ignored: u64::from(n),
        exit: StepExit::Exited { code: i32::from(n) },
        output: RecordedOutput::Split {
            stdout: vec![n; 3],
            stderr: Vec::new(),
        },
        duration_nanos: u64::from(n),
        ran_at_nanos: u64::from(n),
        reason: RunReason::NeverRun,
        recorded_at_nanos: u64::from(n),
        failure: None,
        not_cached: None,
        owners: [task(&format!("/w/{n}"), u32::from(n)), task("/w/shared", 0)].into(),
    }
}

/// A task in `<dir>/kizz.md`.
fn task(
    dir: &str,
    index: u32,
) -> TaskId {
    TaskId {
        file: PathBuf::from(dir).join("kizz.md"),
        index,
    }
}

/// A task record whose hash derives from `n`.
fn task_record(n: u8) -> TaskRecord {
    TaskRecord {
        program_hash: [n; 32],
        succeeded: n.is_multiple_of(2),
        build: BuildId(u64::from(n)),
        steps: 1,
        line: u32::from(n),
        heading: None,
        targets: vec!["ci".to_owned()],
        block: ContentHash([n; 32]),
        modules: Vec::new(),
    }
}

/// The step position of the step whose key derives from `n`, found cached.
fn step_position(n: u8) -> StepPosition {
    StepPosition {
        step: StepKey([n; 32]),
        build: BuildId(u64::from(n)),
        verdict: Verdict::Cached,
        carried: Vec::new(),
    }
}

/// The work of a helper mode, run by this test executable when a test starts it with
/// [`HELPER_VARIABLE`] set. In a normal test run it does nothing.
///
/// The only mode, `crash`, commits three step records, writes a fourth without
/// committing it, reports `committed`, and then waits to be killed.
#[test]
fn helper_process() {
    let Some(mode) = std::env::var_os(HELPER_VARIABLE) else {
        return;
    };
    assert_eq!(mode, "crash");
    let cache = PathBuf::from(std::env::var_os(CACHE_VARIABLE).unwrap());
    let store = Store::open(&cache).unwrap();
    let mut txn = store.write().unwrap();
    for n in 1..=3 {
        txn.put(&StepKey([n; 32]), &step_record(n)).unwrap();
    }
    txn.commit().unwrap();
    let mut pending = store.write().unwrap();
    pending.put(&StepKey([4; 32]), &step_record(4)).unwrap();
    println!("{MARK}committed");
    loop {
        thread::park();
    }
}

#[test]
fn records_survive_closing_and_reopening_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let store = Store::open(&cache).unwrap();
    assert_eq!(store.path(), cache.join(DATABASE_DIR));
    let mut txn = store.write().unwrap();
    txn.put(&StepKey([1; 32]), &step_record(1)).unwrap();
    let mut unowned = step_record(2);
    unowned.owners.clear();
    txn.put(&StepKey([2; 32]), &unowned).unwrap();
    txn.put(&task("/w/a", 0), &task_record(2)).unwrap();
    txn.put(&PathKey(PathBuf::from("/w/a/out")), &LastWriter {
        task: task("/w/a", 0),
        by: WrittenBy::Step {
            step: StepKey([1; 32]),
            position: 0,
        },
        build: BuildId(1),
        written_at_nanos: 1,
    })
    .unwrap();
    txn.put(&PathKey(PathBuf::from("/w/a/notes")), &LastWriter {
        task: task("/w/a", 0),
        by: WrittenBy::Program,
        build: BuildId(1),
        written_at_nanos: 2,
    })
    .unwrap();
    txn.put(&task("/w/b", 0), &LearnedOrder {
        producers: [task("/w/a", 0)].into(),
    })
    .unwrap();
    txn.put(
        &PositionKey {
            task: task("/w/a", 0),
            position: 0,
        },
        &step_position(1),
    )
    .unwrap();
    txn.commit().unwrap();
    store.close().unwrap();

    let store = Store::open(&cache).unwrap();
    let txn = store.read().unwrap();
    let owned = txn.get::<StepRecord>(&StepKey([1; 32])).unwrap();
    assert_eq!(owned, Some(step_record(1)));
    assert_eq!(
        owned.map(|record| record.owners.into_iter().collect::<Vec<_>>()),
        Some(vec![task("/w/1", 1), task("/w/shared", 0)])
    );
    assert_eq!(txn.get(&StepKey([2; 32])).unwrap(), Some(unowned));
    assert_eq!(txn.get(&task("/w/a", 0)).unwrap(), Some(task_record(2)));
    assert_eq!(
        txn.get::<LastWriter>(&PathKey(PathBuf::from("/w/a/out")))
            .unwrap()
            .map(|writer| writer.by),
        Some(WrittenBy::Step {
            step: StepKey([1; 32]),
            position: 0,
        })
    );
    assert_eq!(
        txn.get::<LastWriter>(&PathKey(PathBuf::from("/w/a/notes")))
            .unwrap(),
        Some(LastWriter {
            task: task("/w/a", 0),
            by: WrittenBy::Program,
            build: BuildId(1),
            written_at_nanos: 2,
        })
    );
    assert_eq!(
        txn.get::<LearnedOrder>(&task("/w/b", 0)).unwrap(),
        Some(LearnedOrder {
            producers: [task("/w/a", 0)].into(),
        })
    );
    assert_eq!(
        txn.get::<StepPosition>(&PositionKey {
            task: task("/w/a", 0),
            position: 0,
        })
        .unwrap(),
        Some(step_position(1))
    );
    assert_eq!(
        txn.get::<SchemaVersion>(&SchemaKey).unwrap(),
        Some(SchemaVersion {
            version: SCHEMA_VERSION,
        })
    );
    drop(txn);
    store.close().unwrap();
}

#[test]
fn committed_records_survive_a_crash_and_uncommitted_ones_do_not() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "helper_process",
            "--nocapture",
            "--test-threads=1",
            "--quiet",
        ])
        .env_clear()
        .env(HELPER_VARIABLE, "crash")
        .env(CACHE_VARIABLE, &cache)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let reported = stdout
        .lines()
        .map(Result::unwrap)
        .find_map(|line| line.strip_prefix(MARK).map(str::to_owned));
    assert_eq!(reported.as_deref(), Some("committed"));
    // `Child::kill` sends SIGKILL: the store is never closed.
    child.kill().unwrap();
    child.wait().unwrap();

    let store = Store::open(&cache).unwrap();
    let txn = store.read().unwrap();
    for n in 1..=3 {
        assert_eq!(txn.get(&StepKey([n; 32])).unwrap(), Some(step_record(n)));
    }
    assert_eq!(txn.get::<StepRecord>(&StepKey([4; 32])).unwrap(), None);
    drop(txn);
    store.close().unwrap();
}

#[test]
fn an_uncommitted_transaction_leaves_no_record() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut txn = store.write().unwrap();
    txn.put(&StepKey([1; 32]), &step_record(1)).unwrap();
    txn.put(&task("/w/a", 0), &task_record(1)).unwrap();
    // A transaction sees its own writes before it commits.
    assert_eq!(txn.get(&StepKey([1; 32])).unwrap(), Some(step_record(1)));
    drop(txn);

    let txn = store.read().unwrap();
    assert_eq!(txn.get::<StepRecord>(&StepKey([1; 32])).unwrap(), None);
    assert_eq!(txn.get::<TaskRecord>(&task("/w/a", 0)).unwrap(), None);
    drop(txn);
    store.close().unwrap();

    let store = Store::open(dir.path()).unwrap();
    let txn = store.read().unwrap();
    assert_eq!(txn.scan::<StepRecord>(b"").unwrap(), Vec::new());
    assert_eq!(txn.scan::<TaskRecord>(b"").unwrap(), Vec::new());
}

#[test]
fn a_committed_transaction_is_all_or_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();

    // Both transactions start from the same snapshot and write the same step record.
    let mut first = store.write().unwrap();
    let mut second = store.write().unwrap();
    first.put(&StepKey([1; 32]), &step_record(1)).unwrap();
    first.put(&task("/w/a", 0), &task_record(1)).unwrap();
    second.put(&StepKey([1; 32]), &step_record(2)).unwrap();
    second.put(&task("/w/b", 0), &task_record(2)).unwrap();
    second.put(&StepKey([2; 32]), &step_record(2)).unwrap();

    first.commit().unwrap();
    assert!(matches!(second.commit(), Err(StoreError::Conflict)));

    // Every record of the first is there, and none of the second.
    let txn = store.read().unwrap();
    assert_eq!(txn.get(&StepKey([1; 32])).unwrap(), Some(step_record(1)));
    assert_eq!(txn.get(&task("/w/a", 0)).unwrap(), Some(task_record(1)));
    assert_eq!(txn.get::<TaskRecord>(&task("/w/b", 0)).unwrap(), None);
    assert_eq!(txn.get::<StepRecord>(&StepKey([2; 32])).unwrap(), None);
    drop(txn);
    store.close().unwrap();

    // The same holds after reopening.
    let store = Store::open(dir.path()).unwrap();
    let txn = store.read().unwrap();
    assert_eq!(txn.scan::<StepRecord>(b"").unwrap(), vec![(
        StepKey([1; 32]),
        step_record(1)
    )]);
    assert_eq!(txn.scan::<TaskRecord>(b"").unwrap(), vec![(
        task("/w/a", 0),
        task_record(1)
    )]);
}

#[test]
fn a_prefix_scan_returns_exactly_the_records_under_the_prefix_in_key_order() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut txn = store.write().unwrap();
    // Written out of order, with neighbors that share a textual prefix, and records
    // of other families keyed by the same paths.
    let tasks = [
        task("/w/b", 0),
        task("/w/a/sub", 1),
        task("/w/ab", 0),
        task("/w/a", 2),
        task("/w", 0),
        task("/w/a", 0),
        task("/w/a/sub", 0),
    ];
    for (n, id) in (1..).zip(&tasks) {
        txn.put(id, &task_record(n)).unwrap();
        txn.put(id, &LearnedOrder {
            producers: [id.clone()].into(),
        })
        .unwrap();
    }
    txn.put(&task("/w/a", 1), &task_record(99)).unwrap();
    txn.commit().unwrap();
    let mut txn = store.write().unwrap();
    txn.delete::<TaskRecord>(&task("/w/a", 1)).unwrap();
    txn.commit().unwrap();

    let txn = store.read().unwrap();
    let under_a: Vec<TaskId> = txn
        .scan::<TaskRecord>(&path_prefix(Path::new("/w/a")))
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(under_a, vec![
        task("/w/a", 0),
        task("/w/a", 2),
        task("/w/a/sub", 0),
        task("/w/a/sub", 1),
    ]);

    let all: Vec<(TaskId, TaskRecord)> = txn.scan::<TaskRecord>(b"").unwrap();
    let mut expected: Vec<(TaskId, TaskRecord)> = (1..)
        .zip(&tasks)
        .map(|(n, id)| (id.clone(), task_record(n)))
        .collect();
    // Keys sort by their bytes.
    expected.sort_by_key(|(id, _)| id.key_prefix());
    assert_eq!(all, expected);

    // The positions of one task, and nothing of a task whose path extends it.
    let mut write = store.write().unwrap();
    for (position, step) in [(2, 3), (0, 1), (256, 4), (1, 2)] {
        write
            .put(
                &PositionKey {
                    task: task("/w/a", 0),
                    position,
                },
                &step_position(step),
            )
            .unwrap();
    }
    write
        .put(
            &PositionKey {
                task: task("/w/a", 1),
                position: 0,
            },
            &step_position(9),
        )
        .unwrap();
    write.commit().unwrap();
    let txn = store.read().unwrap();
    let positions: Vec<(u32, u8)> = txn
        .scan::<StepPosition>(&task("/w/a", 0).key_prefix())
        .unwrap()
        .into_iter()
        .map(|(key, record)| (key.position, record.step.0[0]))
        .collect();
    assert_eq!(positions, vec![(0, 1), (1, 2), (2, 3), (256, 4)]);
}

#[test]
fn a_schema_version_mismatch_is_refused_and_the_cache_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut txn = store.write().unwrap();
    let other = SCHEMA_VERSION.checked_add(1).unwrap();
    txn.put(&SchemaKey, &SchemaVersion { version: other })
        .unwrap();
    txn.put(&StepKey([1; 32]), &step_record(1)).unwrap();
    txn.commit().unwrap();
    store.close().unwrap();

    for _ in 0..2 {
        match Store::open(dir.path()) {
            Err(StoreError::SchemaMismatch {
                path,
                found,
                expected,
            }) => {
                assert_eq!(path, dir.path().join(DATABASE_DIR));
                assert_eq!(found, other);
                assert_eq!(expected, SCHEMA_VERSION);
            },
            other => panic!("expected a schema mismatch, got {other:?}"),
        }
    }
    let message = Store::open(dir.path()).unwrap_err().to_string();
    assert!(
        message.contains(&format!("schema version {other}")),
        "{message}"
    );
    assert!(
        message.contains(&format!("schema version {SCHEMA_VERSION}")),
        "{message}"
    );
    assert!(message.contains("kizz cache reset --all"), "{message}");
}

/// The inventory counts every family and the distinct tasks of a cache of another
/// schema version without decoding a record, and a wipe empties it, so the next open
/// creates an empty database with the current version. A cache without a database
/// has no entries, and neither the inventory nor the wipe creates one.
#[test]
fn the_inventory_counts_any_cache_and_a_wipe_empties_it() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(Store::inventory(dir.path()).unwrap(), Inventory::default());
    Store::wipe(dir.path()).unwrap();
    assert!(!dir.path().join(DATABASE_DIR).exists());

    let store = Store::open(dir.path()).unwrap();
    let mut txn = store.write().unwrap();
    let other = SCHEMA_VERSION.checked_add(1).unwrap();
    txn.put(&SchemaKey, &SchemaVersion { version: other })
        .unwrap();
    for n in 1..=3 {
        txn.put(&StepKey([n; 32]), &step_record(n)).unwrap();
    }
    txn.put(&task("/w/a", 0), &task_record(1)).unwrap();
    txn.put(&task("/w/b", 0), &LearnedOrder {
        producers: [task("/w/a", 0)].into(),
    })
    .unwrap();
    for (id, position) in [
        (task("/w/a", 0), 0),
        (task("/w/a", 0), 1),
        (task("/w/c", 2), 0),
    ] {
        txn.put(&PositionKey { task: id, position }, &step_position(1))
            .unwrap();
    }
    txn.put(&PathKey(PathBuf::from("/w/a/out")), &LastWriter {
        task: task("/w/a", 0),
        by: WrittenBy::Step {
            step: StepKey([1; 32]),
            position: 0,
        },
        build: BuildId(1),
        written_at_nanos: 1,
    })
    .unwrap();
    txn.commit().unwrap();
    store.close().unwrap();

    let inventory = Store::inventory(dir.path()).unwrap();
    assert_eq!(inventory, Inventory {
        entries: EntryCounts {
            tasks: 3,
            steps: 3,
            shared_steps: 0,
            positions: 3,
            task_records: 1,
            orders: 1,
            writers: 1,
        },
        schema_version: Some(other),
    });
    assert!(!inventory.entries.is_empty());
    // Counting changed nothing: the cache is still refused.
    assert!(matches!(
        Store::open(dir.path()),
        Err(StoreError::SchemaMismatch { .. })
    ));

    Store::wipe(dir.path()).unwrap();
    assert!(!dir.path().join(DATABASE_DIR).exists());
    assert_eq!(Store::inventory(dir.path()).unwrap(), Inventory::default());
    let store = Store::open(dir.path()).unwrap();
    let txn = store.read().unwrap();
    assert_eq!(
        txn.get::<SchemaVersion>(&SchemaKey).unwrap(),
        Some(SchemaVersion {
            version: SCHEMA_VERSION,
        })
    );
    assert_eq!(txn.scan::<StepRecord>(b"").unwrap(), Vec::new());
    drop(txn);
    store.close().unwrap();
    assert_eq!(Store::inventory(dir.path()).unwrap(), Inventory {
        entries: EntryCounts::default(),
        schema_version: Some(SCHEMA_VERSION),
    });
}

#[test]
fn threads_of_one_process_share_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    thread::scope(|scope| {
        for worker in 0..4_u8 {
            let store = &store;
            scope.spawn(move || {
                for n in 0..25_u8 {
                    let mut key = [worker; 32];
                    key[1] = n;
                    let mut txn = store.write().unwrap();
                    txn.put(&StepKey(key), &step_record(n)).unwrap();
                    txn.commit().unwrap();
                }
            });
        }
    });
    let txn = store.read().unwrap();
    let records = txn.scan::<StepRecord>(b"").unwrap();
    assert_eq!(records.len(), 100);
    for (key, record) in records {
        assert_eq!(record, step_record(key.0[1]));
    }
}
