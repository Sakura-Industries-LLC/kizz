// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Cache cleanup in temporary caches: what `clean` removes because the files are
//! gone and which owners it takes out of the records it keeps, and what
//! `plan_reset` selects for the tasks under a directory, by the owners of the step
//! records. Last writers that name a step and those that name a task program are
//! mixed, and both are cleaned and reset by path alike.
//!
//! The tests write the records straight into a store in a temporary cache, and
//! `clean` checks real paths in a temporary directory. The tests of owners that the
//! engine stores are in `tests/owners.rs`.

use std::{
    collections::BTreeSet,
    error::Error,
    fs,
    path::{Path, PathBuf},
};

use kizz_core::{clean, plan_reset};
use kizz_store::{
    AccessBits, BuildId, Capture, ContentHash, EntryCounts, FileMeta, LastWriter, LearnedOrder,
    Observation, PathKey, PathState, PositionKey, RecordedOutput, RunReason, StepExit, StepKey,
    StepPosition, StepRecord, StepSpecRecord, Store, TaskId, TaskRecord, Verdict, WrittenBy,
};

/// The result of a test.
type TestResult = Result<(), Box<dyn Error>>;

/// An observation of `path` in `state`, read.
fn observation(
    path: &Path,
    state: PathState,
) -> Observation {
    Observation {
        path: path.to_path_buf(),
        access: AccessBits(AccessBits::READ),
        tracked: false,
        state,
    }
}

/// The state of a regular file that was read.
fn file() -> PathState {
    PathState::File {
        meta: FileMeta {
            size: 1,
            mtime_nanos: 1,
            ctime_nanos: 1,
            inode: 1,
            device: 1,
            mode: 0o100_644,
        },
        hash: Some(ContentHash([1; 32])),
    }
}

/// A step record that ran in `cwd`, made `observations`, and has no owner.
fn step(
    cwd: &Path,
    observations: Vec<Observation>,
) -> StepRecord {
    StepRecord {
        spec: StepSpecRecord {
            program_hash: ContentHash([0; 32]),
            cwd: cwd.to_path_buf(),
            argv: vec!["tool".to_owned()],
            env: [].into(),
            stdin: None,
            capture: Capture::Split,
            stream: false,
            always: false,
            ignore: Vec::new(),
            track: Vec::new(),
            expect: None,
            occurrence: 0,
        },
        program: None,
        observations,
        ignored: 0,
        exit: StepExit::Exited { code: 0 },
        output: RecordedOutput::Split {
            stdout: Vec::new(),
            stderr: Vec::new(),
        },
        duration_nanos: 1,
        ran_at_nanos: 1,
        reason: RunReason::NeverRun,
        recorded_at_nanos: 1,
        failure: None,
        not_cached: None,
        owners: BTreeSet::new(),
    }
}

/// `record`, owned by `owners`.
fn owned(
    mut record: StepRecord,
    owners: &[&TaskId],
) -> StepRecord {
    record.owners = owners.iter().map(|&owner| owner.clone()).collect();
    record
}

/// The owners of the step record under `key`, in order.
fn owners_of(
    store: &Store,
    key: &StepKey,
) -> Result<Vec<TaskId>, Box<dyn Error>> {
    let record = store
        .read()?
        .get::<StepRecord>(key)?
        .ok_or("no step record")?;
    Ok(record.owners.into_iter().collect())
}

/// The task with index `index` in `<dir>/kizz.md`.
fn task_at(
    dir: &Path,
    index: u32,
) -> TaskId {
    TaskId {
        file: dir.join("kizz.md"),
        index,
    }
}

/// The first task in `<dir>/kizz.md`.
fn task(dir: &Path) -> TaskId {
    TaskId {
        file: dir.join("kizz.md"),
        index: 0,
    }
}

/// A task record of one step.
fn task_record() -> TaskRecord {
    TaskRecord {
        program_hash: [0; 32],
        succeeded: true,
        build: BuildId(1),
        steps: 1,
        line: 3,
        heading: None,
        targets: vec!["ci".to_owned()],
        block: ContentHash([0; 32]),
        modules: Vec::new(),
    }
}

/// The step position that holds `step`.
fn position(step: StepKey) -> StepPosition {
    StepPosition {
        step,
        build: BuildId(1),
        verdict: Verdict::Cached,
        carried: Vec::new(),
    }
}

/// A last writer that is a step of a task.
const BY_STEP: WrittenBy = WrittenBy::Step {
    step: StepKey([0; 32]),
    position: 0,
};

/// The last-writer entry of `by`, a step or the program of `task`.
fn writer(
    task: &TaskId,
    by: WrittenBy,
) -> LastWriter {
    LastWriter {
        task: task.clone(),
        by,
        build: BuildId(1),
        written_at_nanos: 1,
    }
}

/// Writes a task record, a step position that holds `step`, and learned order for
/// `id`.
fn put_task(
    txn: &mut kizz_store::Txn<'_>,
    id: &TaskId,
    step: StepKey,
    producers: &[&TaskId],
) -> Result<(), Box<dyn Error>> {
    txn.put(id, &task_record())?;
    txn.put(
        &PositionKey {
            task: id.clone(),
            position: 0,
        },
        &position(step),
    )?;
    if !producers.is_empty() {
        txn.put(id, &LearnedOrder {
            producers: producers.iter().map(|&producer| producer.clone()).collect(),
        })?;
    }
    Ok(())
}

/// The keys of every entry of each family, in key order.
#[derive(Debug)]
struct Keys {
    /// Step records.
    steps: Vec<StepKey>,
    /// Step positions.
    positions: Vec<PositionKey>,
    /// Task records.
    tasks: Vec<TaskId>,
    /// Learned order.
    orders: Vec<TaskId>,
    /// The paths of last writers.
    writers: Vec<PathBuf>,
}

/// The keys of every entry in `store`.
fn keys(store: &Store) -> Result<Keys, Box<dyn Error>> {
    /// The keys of the entries of a scan.
    fn of<K, R>(entries: Vec<(K, R)>) -> Vec<K> {
        entries.into_iter().map(|(key, _)| key).collect()
    }
    let txn = store.read()?;
    Ok(Keys {
        steps: of(txn.scan::<StepRecord>(&[])?),
        positions: of(txn.scan::<StepPosition>(&[])?),
        tasks: of(txn.scan::<TaskRecord>(&[])?),
        orders: of(txn.scan::<LearnedOrder>(&[])?),
        writers: of(txn.scan::<LastWriter>(&[])?)
            .into_iter()
            .map(|key| key.0)
            .collect(),
    })
}

/// The step record of the clean test that `keep`'s task and the gone task own.
const KEPT_STEP: StepKey = StepKey([1; 32]);
/// The step record of the clean test that depends on a file of the gone project.
const ON_GONE_FILE: StepKey = StepKey([2; 32]);
/// The step record of the clean test that ran in the gone project.
const IN_GONE_DIR: StepKey = StepKey([3; 32]);
/// The step record of the clean test that depends on paths that never existed.
const ON_ABSENT_PATHS: StepKey = StepKey([4; 32]);
/// The step record of the clean test that depends on a file that is deleted.
const ON_DELETED_FILE: StepKey = StepKey([5; 32]);

/// Writes the entries of the clean test for the projects `keep` and `gone`.
fn put_clean_entries(
    store: &Store,
    keep: &Path,
    gone: &Path,
) -> TestResult {
    let mut txn = store.write()?;
    let in_keep = observation(&keep.join("in.txt"), file());
    let never = observation(&keep.join("never.txt"), PathState::Absent);
    let untracked = observation(Path::new("/proc/4294967295/stat"), PathState::Untracked);
    let both = [&task(keep), &task(gone)];
    txn.put(
        &KEPT_STEP,
        &owned(step(keep, vec![in_keep, never, untracked]), &both),
    )?;
    let on_gone = vec![observation(&gone.join("in.txt"), file())];
    txn.put(&ON_GONE_FILE, &owned(step(keep, on_gone), &[&task(keep)]))?;
    txn.put(&IN_GONE_DIR, &owned(step(gone, Vec::new()), &[&task(gone)]))?;
    let absent = vec![
        observation(&gone.join("never.txt"), PathState::Absent),
        observation(&keep.join("no/such/file"), PathState::Absent),
    ];
    txn.put(&ON_ABSENT_PATHS, &owned(step(keep, absent), &[&task(gone)]))?;
    let deleted = vec![observation(&keep.join("deleted.txt"), file())];
    txn.put(
        &ON_DELETED_FILE,
        &owned(step(keep, deleted), &[&task(keep)]),
    )?;
    put_task(&mut txn, &task(keep), KEPT_STEP, &[&task(gone)])?;
    put_task(&mut txn, &task(gone), IN_GONE_DIR, &[&task(keep)])?;
    for (path, by) in [
        (keep.join("in.txt"), WrittenBy::Program),
        (keep.join("deleted.txt"), BY_STEP),
        (gone.join("in.txt"), WrittenBy::Program),
    ] {
        txn.put(&PathKey(path), &writer(&task(keep), by))?;
    }
    txn.commit()?;
    Ok(())
}

/// `clean` removes, as whole records, the steps that depend on a path or a working
/// directory that existed and is gone, and the task records, step positions, learned
/// order, and last writers for `kizz.md` files and paths that are gone. From the step
/// records it keeps, it takes out the owners whose `kizz.md` is gone and keeps the
/// others, and it keeps a record left with no owner while its dependencies exist. It
/// keeps dependencies on paths that never existed and untracked `/proc` accesses, and
/// it changes no file.
#[test]
fn clean_removes_entries_about_gone_paths_and_keeps_the_rest() -> TestResult {
    let root_dir = tempfile::tempdir()?;
    let root = root_dir.path().canonicalize()?;
    let keep = root.join("keep");
    let gone = root.join("gone");
    for dir in [&keep, &gone] {
        fs::create_dir_all(dir)?;
        fs::write(dir.join("kizz.md"), "# Task\n")?;
        fs::write(dir.join("in.txt"), "in\n")?;
    }
    fs::write(keep.join("deleted.txt"), "deleted\n")?;
    let cache = tempfile::tempdir()?;
    let store = Store::open(cache.path())?;

    put_clean_entries(&store, &keep, &gone)?;

    fs::remove_dir_all(&gone)?;
    fs::remove_file(keep.join("deleted.txt"))?;
    let cleaned = clean(&store)?;
    assert_eq!(cleaned.removed, EntryCounts {
        tasks: 1,
        steps: 3,
        shared_steps: 2,
        positions: 1,
        task_records: 1,
        orders: 1,
        writers: 2,
    });
    // `gone/in.txt`, `gone/kizz.md`, `gone`, and `keep/deleted.txt`.
    assert_eq!(cleaned.gone, 4);
    assert!(cleaned.checked > cleaned.gone);

    let Keys {
        steps,
        positions,
        tasks,
        orders,
        writers,
    } = keys(&store)?;
    assert_eq!(steps, [KEPT_STEP, ON_ABSENT_PATHS]);
    // The kept records lost the owner whose `kizz.md` is gone, and the one it owned
    // alone is kept with no owner, since its dependencies exist.
    assert_eq!(owners_of(&store, &KEPT_STEP)?, [task(&keep)]);
    assert_eq!(owners_of(&store, &ON_ABSENT_PATHS)?, Vec::<TaskId>::new());
    assert_eq!(positions, [PositionKey {
        task: task(&keep),
        position: 0,
    }]);
    assert_eq!(tasks, [task(&keep)]);
    // The kept task's learned order still names the gone task: it is the kept
    // task's entry.
    assert_eq!(orders, [task(&keep)]);
    assert_eq!(writers, [keep.join("in.txt")]);
    let mut files: Vec<PathBuf> = fs::read_dir(&keep)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<_, _>>()?;
    files.sort();
    assert_eq!(files, [keep.join("in.txt"), keep.join("kizz.md")]);

    // Nothing is left to clean.
    assert_eq!(clean(&store)?.removed, EntryCounts::default());
    store.close()?;
    Ok(())
}

/// The reset test's record of the task in `sub`, which its position holds.
const SUB_STEP: StepKey = StepKey([1; 32]);
/// The reset test's record of the top task that ran in `sub`.
const TOP_STEP_IN_SUB: StepKey = StepKey([2; 32]);
/// The reset test's record shared by the tasks in `sub` and at the top.
const SHARED_STEP: StepKey = StepKey([3; 32]);
/// The reset test's historical record of the task in `sub`, which ran elsewhere.
const SUB_HISTORY_ELSEWHERE: StepKey = StepKey([4; 32]);
/// The reset test's record owned only by tasks under `sub` without other entries.
const OWNED_ONLY_UNDER_SUB: StepKey = StepKey([5; 32]);
/// The reset test's record with no owner, which ran in `sub`.
const UNOWNED_IN_SUB: StepKey = StepKey([6; 32]);
/// The reset test's record of the task in `subway`.
const SUBWAY_STEP: StepKey = StepKey([7; 32]);
/// The reset test's historical record of the top task, which ran under `sub`.
const TOP_HISTORY_IN_SUB: StepKey = StepKey([8; 32]);

/// Writes the entries of the reset test under `root`.
fn put_reset_entries(
    store: &Store,
    root: &Path,
) -> TestResult {
    let sub = root.join("sub");
    let subway = root.join("subway");
    let (top, in_sub, in_subway) = (task(root), task(&sub), task(&subway));
    let only = task_at(&sub.join("only"), 0);
    let deep_owner = task_at(&sub.join("deep"), 2);
    let mut txn = store.write()?;
    let records = [
        (SUB_STEP, step(&sub, Vec::new()), vec![&in_sub]),
        (TOP_STEP_IN_SUB, step(&sub, Vec::new()), vec![&top]),
        (SHARED_STEP, step(&root.join("common"), Vec::new()), vec![
            &in_sub, &top,
        ]),
        (
            SUB_HISTORY_ELSEWHERE,
            step(Path::new("/elsewhere"), Vec::new()),
            vec![&in_sub],
        ),
        (OWNED_ONLY_UNDER_SUB, step(root, Vec::new()), vec![
            &only,
            &deep_owner,
        ]),
        (UNOWNED_IN_SUB, step(&sub, Vec::new()), Vec::new()),
        (SUBWAY_STEP, step(&subway, Vec::new()), vec![&in_subway]),
        (TOP_HISTORY_IN_SUB, step(&sub.join("x"), Vec::new()), vec![
            &top,
        ]),
    ];
    for (key, record, owners) in records {
        txn.put(&key, &owned(record, &owners))?;
    }
    put_task(&mut txn, &in_sub, SUB_STEP, &[&top])?;
    put_task(&mut txn, &top, TOP_STEP_IN_SUB, &[&in_sub])?;
    put_task(&mut txn, &in_subway, SUBWAY_STEP, &[])?;
    txn.put(
        &PositionKey {
            task: task(&sub.join("deep")),
            position: 3,
        },
        &position(SUB_STEP),
    )?;
    for (path, by) in [
        (sub.join("out"), WrittenBy::Program),
        (subway.join("out"), BY_STEP),
        (root.join("out"), WrittenBy::Program),
    ] {
        txn.put(&PathKey(path), &writer(&in_sub, by))?;
    }
    txn.commit()?;
    Ok(())
}

/// `plan_reset` selects the task records, step positions, learned order, and last
/// writers under the directory, and the step records by their owners alone: a record
/// whose owners are all under the directory is removed, whatever its working
/// directory and whether or not a step position holds it; a record that also has an
/// owner outside it only loses the owners under it; and a record with no owner, or
/// owned only by tasks outside, is kept unchanged even when it ran under the
/// directory. It selects nothing for a sibling whose name only starts with the
/// directory's. Its counts count the removed step records and, apart from them, the
/// shared records, and every task under the directory, those that only own step
/// records included. Applying the plan removes and changes exactly those.
#[test]
fn a_reset_plan_selects_exactly_the_entries_for_tasks_under_the_directory() -> TestResult {
    let root = Path::new("/w");
    let sub = root.join("sub");
    let subway = root.join("subway");
    let cache = tempfile::tempdir()?;
    let store = Store::open(cache.path())?;
    put_reset_entries(&store, root)?;

    let plan = plan_reset(&store, &sub)?;
    assert_eq!(plan.dir(), sub.as_path());
    // The tasks: `sub`, the task in `sub/deep` with a position, and the two tasks
    // that only own a step record.
    assert_eq!(plan.counts(), EntryCounts {
        tasks: 4,
        steps: 3,
        shared_steps: 1,
        positions: 2,
        task_records: 1,
        orders: 1,
        writers: 1,
    });
    // Planning changes nothing.
    assert_eq!(keys(&store)?.steps.len(), 8);
    assert_eq!(owners_of(&store, &SHARED_STEP)?, [task(root), task(&sub)]);

    plan.apply(&store)?;
    let Keys {
        steps,
        positions,
        tasks,
        orders,
        writers,
    } = keys(&store)?;
    assert_eq!(steps, [
        TOP_STEP_IN_SUB,
        SHARED_STEP,
        UNOWNED_IN_SUB,
        SUBWAY_STEP,
        TOP_HISTORY_IN_SUB
    ]);
    assert_eq!(owners_of(&store, &SHARED_STEP)?, [task(root)]);
    assert_eq!(owners_of(&store, &TOP_STEP_IN_SUB)?, [task(root)]);
    assert_eq!(owners_of(&store, &TOP_HISTORY_IN_SUB)?, [task(root)]);
    assert_eq!(owners_of(&store, &UNOWNED_IN_SUB)?, Vec::<TaskId>::new());
    assert_eq!(positions, [
        PositionKey {
            task: task(root),
            position: 0,
        },
        PositionKey {
            task: task(&subway),
            position: 0,
        },
    ]);
    assert_eq!(tasks, [task(root), task(&subway)]);
    assert_eq!(orders, [task(root)]);
    assert_eq!(writers, [root.join("out"), subway.join("out")]);

    // Nothing is left under the directory: the record with no owner that ran in it
    // is no task's entry.
    assert_eq!(plan_reset(&store, &sub)?.counts(), EntryCounts::default());

    // A reset of the parent removes the records its tasks own, and still keeps the
    // record with no owner.
    plan_reset(&store, root)?.apply(&store)?;
    assert_eq!(keys(&store)?.steps, [UNOWNED_IN_SUB]);
    assert_eq!(plan_reset(&store, root)?.counts(), EntryCounts::default());
    store.close()?;
    Ok(())
}

/// A reset whose only change is to the owners of a shared step record is not
/// empty: it counts the shared record and the task under the directory, and
/// applying it takes only that task out of the owners.
#[test]
fn a_reset_that_only_changes_owners_is_not_empty() -> TestResult {
    let root = Path::new("/w");
    let sub = root.join("sub");
    let cache = tempfile::tempdir()?;
    let store = Store::open(cache.path())?;
    let shared = StepKey([1; 32]);
    let mut txn = store.write()?;
    txn.put(
        &shared,
        &owned(step(&sub, Vec::new()), &[&task(&sub), &task(root)]),
    )?;
    txn.commit()?;

    let plan = plan_reset(&store, &sub)?;
    let counts = plan.counts();
    assert_eq!(counts, EntryCounts {
        tasks: 1,
        shared_steps: 1,
        ..EntryCounts::default()
    });
    assert!(!counts.is_empty());
    plan.apply(&store)?;
    assert_eq!(keys(&store)?.steps, [shared]);
    assert_eq!(owners_of(&store, &shared)?, [task(root)]);
    assert!(plan_reset(&store, &sub)?.counts().is_empty());
    store.close()?;
    Ok(())
}
