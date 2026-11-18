// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Explaining decisions: why a step ran ("Explaining decisions").
//!
//! The engine turns the cache check's [`RunReason`](kizz_store::RunReason) into an
//! [`Explanation`](kizz_store::Explanation) before a step runs:
//!
//! * A step key with no record is explained through the step-position records: when the
//!   task used another key at the same position last time, the reasons compare that key's
//!   specification with this one. The task program, the arguments, the environment, the
//!   working directory, `stdin`, the capture mode, the ignore patterns, the `track`
//!   patterns, the `timestamps` option, and the `machine` names each give their own
//!   reason; a changed program names its block or each loaded module that changed, from
//!   the part hashes of the task record. Without an earlier specification, or when only
//!   the occurrence number differs, the step never ran.
//! * A step whose record names parts of the machine state, and whose cache check found
//!   values of them changed, gives a reason of its own that names each changed value, old
//!   and new, after the changed inputs.
//! * Each changed input names who last wrote it, from the last-writer records: this
//!   command on its previous run, another step in this build, a step of an earlier build,
//!   the program of a task, through `fs.write` or `fs.write_json`, in this build or an
//!   earlier one, or something outside kizz. A write that kizz recorded before the step's
//!   recorded state was taken cannot have changed the path since, so then the writer is
//!   something outside kizz. A task program is named as the writer even when it is the
//!   program of the step's own task.
//! * A changed input is self-inflicted when this command last wrote it on its previous
//!   run and also reads it, or when the step that wrote it in this build runs on every
//!   build, because it is marked `always`, its task is marked `cache=never`, or it
//!   re-runs for a self-inflicted reason itself. A path the command read and wrote before
//!   is not self-inflicted when something outside kizz, another task, or a task program
//!   changed it since; a change that a task program made is never self-inflicted.
//! * A step that re-runs for a self-inflicted input that also made it run in its task's
//!   previous build gets the loud warning for consecutive builds: the step-position
//!   record carries the self-inflicted inputs of the previous build's run.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use kizz_store::{
    AccessBits, BuildId, ChangedInput, ContentHash, Difference, EnvChange, LastWriter,
    ModuleChange, ModuleChangeKind, ModuleHash, Observation, PathKey, PositionKey, Reason,
    SelfInflicted, StepKey, StepPosition, StepSpecRecord, StoreError, TaskId, TaskLabel,
    TaskRecord, Txn, Verdict, Writer, WrittenBy,
};
use kizz_trace::AbsolutePath;

use crate::runtime::{ProgramHash, ProgramParts, TaskProgram};

/// The identity of a task whose steps the engine runs: what its step positions,
/// last writers, and task record are stored under, and how output names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskIdentity {
    /// The task: the absolute path of its `kizz.md` and its index in the file.
    pub id: TaskId,
    /// The line of the block's opening fence, counting from 1.
    pub line: u32,
    /// The nearest heading above the block, if there is one.
    pub heading: Option<String>,
    /// The targets on the block, in the order written.
    pub targets: Vec<String>,
    /// The hash of the task program.
    pub program: ProgramHash,
    /// The hashes of the program's block and modules.
    pub parts: ProgramParts,
    /// The task directory, at which relative patterns are anchored.
    pub dir: AbsolutePath,
}

impl TaskIdentity {
    /// The identity of the task of the loaded program `program`.
    #[must_use]
    pub fn of(program: &TaskProgram) -> Self {
        let task = program.task();
        Self {
            id: TaskId {
                file: task.file.as_path().to_path_buf(),
                index: task.index,
            },
            line: u32::try_from(task.line).unwrap_or(u32::MAX),
            heading: task.heading.clone(),
            targets: task.targets.clone(),
            program: program.hash(),
            parts: program.parts().clone(),
            dir: task.dir.clone(),
        }
    }

    /// The task as explanations name it.
    #[must_use]
    pub fn label(&self) -> TaskLabel {
        TaskLabel {
            task: self.id.clone(),
            line: self.line,
            heading: self.heading.clone(),
        }
    }

    /// The task record of a run in `build` that had `steps` steps and ended as
    /// `succeeded` says.
    #[must_use]
    pub fn task_record(
        &self,
        succeeded: bool,
        build: BuildId,
        steps: u32,
    ) -> TaskRecord {
        TaskRecord {
            program_hash: *self.program.as_bytes(),
            succeeded,
            build,
            steps,
            line: self.line,
            heading: self.heading.clone(),
            targets: self.targets.clone(),
            block: ContentHash(self.parts.block),
            modules: self
                .parts
                .modules
                .iter()
                .map(|(path, hash)| {
                    ModuleHash {
                        path: path.clone(),
                        hash: ContentHash(*hash),
                    }
                })
                .collect(),
        }
    }
}

/// What a build decided about one step of a task run, for the line `kizz build`
/// prints for each command it runs and for `--explain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepDecision {
    /// The position of the step in the task's run, counting from zero.
    pub position: u32,
    /// The command: the argument vector joined with spaces.
    pub command: String,
    /// What the build decided.
    pub verdict: Verdict,
}

/// The loud warning for a step that re-ran in consecutive builds for the same
/// self-inflicted reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepeatedWarning {
    /// The step's task, or `None` for steps run without a task identity.
    pub task: Option<TaskLabel>,
    /// The position of the step in the task's run, counting from zero.
    pub position: u32,
    /// The command: the argument vector joined with spaces.
    pub command: String,
    /// The self-inflicted inputs that made the step run in both builds, sorted.
    pub paths: Vec<PathBuf>,
}

/// The recorded state a cache check compared the file system with: the paths the
/// step wrote and also read, and when the state was taken.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Baseline {
    /// When kizz began recording the states, in nanoseconds since the Unix epoch.
    pub recorded_at_nanos: u64,
    /// The recorded paths that the step both wrote and read.
    pub writes_and_reads: BTreeSet<PathBuf>,
}

impl Baseline {
    /// The baseline of the observations `observations`, recorded at
    /// `recorded_at_nanos`.
    #[must_use]
    pub fn new(
        observations: &[Observation],
        recorded_at_nanos: u64,
    ) -> Self {
        Self {
            recorded_at_nanos,
            writes_and_reads: observations
                .iter()
                .filter(|observation| {
                    observation.access.contains(AccessBits::WRITE)
                        && observation.access.contains(AccessBits::READ)
                })
                .map(|observation| observation.path.clone())
                .collect(),
        }
    }
}

/// What attributing the changed inputs of one step needs.
#[derive(Debug, Clone, Copy)]
pub struct InputContext<'a> {
    /// The step's task, if it has an identity.
    pub identity: Option<&'a TaskIdentity>,
    /// The position of the step in the task's run.
    pub position: u32,
    /// The step key.
    pub key: StepKey,
    /// The current build.
    pub build: BuildId,
    /// The recorded state the cache check compared the file system with.
    pub baseline: Option<&'a Baseline>,
}

/// The reasons that compare `old`, the specification the task used last time at
/// the same position, with `new`, in the order of the spec's list.
///
/// A changed program names its block and each changed, added, or removed module
/// when `old_task` is the task record of `old`'s program and `new_parts` holds the
/// parts of `new`'s program; otherwise it names no part.
#[must_use]
pub fn spec_reasons(
    old: &StepSpecRecord,
    new: &StepSpecRecord,
    old_task: Option<&TaskRecord>,
    new_parts: Option<&ProgramParts>,
) -> Vec<Reason> {
    let mut reasons = Vec::new();
    if old.program_hash != new.program_hash {
        reasons.push(program_reason(old, old_task, new_parts));
    }
    if old.argv != new.argv {
        reasons.push(Reason::ArgumentsChanged {
            old: old.argv.clone(),
            new: new.argv.clone(),
        });
    }
    let changes = env_changes(&old.env, &new.env);
    if !changes.is_empty() {
        reasons.push(Reason::EnvironmentChanged { changes });
    }
    if old.cwd != new.cwd {
        reasons.push(Reason::WorkingDirectoryChanged {
            old: old.cwd.clone(),
            new: new.cwd.clone(),
        });
    }
    if old.stdin != new.stdin {
        reasons.push(Reason::StdinChanged {
            old: old.stdin.clone(),
            new: new.stdin.clone(),
        });
    }
    if old.capture != new.capture {
        reasons.push(Reason::CaptureChanged {
            old: old.capture,
            new: new.capture,
        });
    }
    if old.ignore != new.ignore {
        reasons.push(Reason::IgnoreChanged {
            old: old.ignore.clone(),
            new: new.ignore.clone(),
        });
    }
    if old.track != new.track {
        reasons.push(Reason::TrackChanged {
            old: old.track.clone(),
            new: new.track.clone(),
        });
    }
    if old.timestamps != new.timestamps {
        reasons.push(Reason::TimestampsChanged {
            old: old.timestamps,
            new: new.timestamps,
        });
    }
    if old.untraced != new.untraced {
        reasons.push(Reason::TracingChanged {
            old: old.untraced,
            new: new.untraced,
        });
    }
    if old.machine != new.machine {
        reasons.push(Reason::MachineNamesChanged {
            old: old.machine.clone(),
            new: new.machine.clone(),
        });
    }
    reasons
}

/// The reason for a changed task program: which parts changed, when the parts of
/// both programs are known.
fn program_reason(
    old: &StepSpecRecord,
    old_task: Option<&TaskRecord>,
    new_parts: Option<&ProgramParts>,
) -> Reason {
    let unknown = Reason::ProgramChanged {
        block: false,
        modules: Vec::new(),
    };
    let (Some(old_task), Some(new_parts)) = (old_task, new_parts) else {
        return unknown;
    };
    if old_task.program_hash != old.program_hash.0 {
        return unknown;
    }
    let old_modules: BTreeMap<&PathBuf, &ContentHash> = old_task
        .modules
        .iter()
        .map(|module| (&module.path, &module.hash))
        .collect();
    let new_modules: BTreeMap<&PathBuf, ContentHash> = new_parts
        .modules
        .iter()
        .map(|(path, hash)| (path, ContentHash(*hash)))
        .collect();
    let paths: BTreeSet<&PathBuf> = old_modules
        .keys()
        .chain(new_modules.keys())
        .copied()
        .collect();
    let modules = paths
        .into_iter()
        .filter_map(|path| {
            let kind = match (old_modules.get(path), new_modules.get(path)) {
                (Some(old), Some(new)) if *old == new => return None,
                (Some(_), Some(_)) => ModuleChangeKind::Changed,
                (None, _) => ModuleChangeKind::Added,
                (Some(_), None) => ModuleChangeKind::Removed,
            };
            Some(ModuleChange {
                path: path.clone(),
                kind,
            })
        })
        .collect();
    Reason::ProgramChanged {
        block: old_task.block.0 != new_parts.block,
        modules,
    }
}

/// Every variable added, removed, or changed between `old` and `new`, sorted by
/// name.
fn env_changes(
    old: &BTreeMap<String, String>,
    new: &BTreeMap<String, String>,
) -> Vec<EnvChange> {
    let names: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    names
        .into_iter()
        .filter_map(|name| {
            let (old, new) = (old.get(name), new.get(name));
            (old != new).then(|| {
                EnvChange {
                    name: name.clone(),
                    old: old.cloned(),
                    new: new.cloned(),
                }
            })
        })
        .collect()
}

/// Every changed input of `differences`, with who last wrote it and whether the
/// change is self-inflicted.
///
/// # Errors
///
/// A [`StoreError`] when a record cannot be read.
pub fn changed_inputs(
    txn: &Txn<'_>,
    context: &InputContext<'_>,
    differences: &[Difference],
) -> Result<Vec<ChangedInput>, StoreError> {
    differences
        .iter()
        .map(|difference| {
            let entry = txn.get::<LastWriter>(&PathKey(difference.path.clone()))?;
            let writer = attribute(txn, context, entry.as_ref())?;
            // The change is this command's own doing only when kizz attributes the
            // write to this command; a path it read and wrote before can still be
            // changed outside kizz or by another task.
            let writes_and_reads = writer == Writer::ThisCommand
                && context
                    .baseline
                    .is_some_and(|baseline| baseline.writes_and_reads.contains(&difference.path));
            let self_inflicted = if writes_and_reads {
                Some(SelfInflicted::WritesAndReads)
            } else if let (Writer::ThisBuild { task, position }, Some(entry)) = (&writer, &entry)
                && writes_every_build(txn, entry, context.build)?
            {
                Some(SelfInflicted::WrittenEveryBuild {
                    task: task.clone(),
                    position: *position,
                })
            } else {
                None
            };
            Ok(ChangedInput {
                path: difference.path.clone(),
                change: difference.change.clone(),
                writer,
                self_inflicted,
            })
        })
        .collect()
}

/// Who last wrote a changed input whose last-writer record is `entry`.
fn attribute(
    txn: &Txn<'_>,
    context: &InputContext<'_>,
    entry: Option<&LastWriter>,
) -> Result<Writer, StoreError> {
    let Some(entry) = entry else {
        return Ok(Writer::Outside);
    };
    let baseline_at = context
        .baseline
        .map_or(0, |baseline| baseline.recorded_at_nanos);
    if entry.written_at_nanos <= baseline_at {
        // The recorded state already includes that write, so whatever changed the
        // path since did so outside kizz.
        return Ok(Writer::Outside);
    }
    let this_build = entry.build == context.build;
    let WrittenBy::Step { step, position } = entry.by else {
        let task = label_of(txn, context.identity, &entry.task)?;
        return Ok(if this_build {
            Writer::ProgramThisBuild { task }
        } else {
            Writer::ProgramEarlierBuild { task }
        });
    };
    let same_position = context
        .identity
        .is_some_and(|identity| identity.id == entry.task && position == context.position);
    if step == context.key || same_position {
        return Ok(Writer::ThisCommand);
    }
    let task = label_of(txn, context.identity, &entry.task)?;
    Ok(if this_build {
        Writer::ThisBuild { task, position }
    } else {
        Writer::EarlierBuild { task, position }
    })
}

/// The label of the task `task`: from `identity` when it is that task, otherwise
/// from its task record, and otherwise with an unknown line.
fn label_of(
    txn: &Txn<'_>,
    identity: Option<&TaskIdentity>,
    task: &TaskId,
) -> Result<TaskLabel, StoreError> {
    if let Some(identity) = identity.filter(|identity| identity.id == *task) {
        return Ok(identity.label());
    }
    Ok(match txn.get::<TaskRecord>(task)? {
        Some(record) => {
            TaskLabel {
                task: task.clone(),
                line: record.line,
                heading: record.heading,
            }
        },
        None => {
            TaskLabel {
                task: task.clone(),
                line: 0,
                heading: None,
            }
        },
    })
}

/// Whether the step that `entry` names, which wrote its path in `build`, runs on
/// every build: in this build it ran because it is marked `always`, because its task
/// is marked `cache=never`, or for a self-inflicted reason. A task program is not a
/// step, so this is never so for one.
fn writes_every_build(
    txn: &Txn<'_>,
    entry: &LastWriter,
    build: BuildId,
) -> Result<bool, StoreError> {
    let WrittenBy::Step { step, position } = entry.by else {
        return Ok(false);
    };
    let position_record = txn.get::<StepPosition>(&PositionKey {
        task: entry.task.clone(),
        position,
    })?;
    Ok(position_record.is_some_and(|record| {
        record.build == build
            && record.step == step
            && matches!(&record.verdict, Verdict::Ran(explanation) if explanation.runs_every_build())
    }))
}

/// The self-inflicted inputs that made the step at a position run in its task's
/// previous build, sorted, given `previous`, the step-position record before this
/// build's decision.
///
/// Within one build the record keeps what it carried; in a new build it carries
/// the self-inflicted inputs of the run it records, and nothing when the step was
/// cached.
#[must_use]
pub fn carried_from(
    previous: Option<&StepPosition>,
    build: BuildId,
) -> Vec<PathBuf> {
    match previous {
        Some(previous) if previous.build == build => previous.carried.clone(),
        Some(StepPosition {
            verdict: Verdict::Ran(explanation),
            ..
        }) => explanation.self_inflicted(),
        Some(_) | None => Vec::new(),
    }
}

/// Unit tests for the comparison of specifications and the carried inputs.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]
mod tests {
    use kizz_store::{Capture, Change, Explanation};

    use super::*;

    /// A specification.
    fn spec() -> StepSpecRecord {
        StepSpecRecord {
            program_hash: ContentHash([1; 32]),
            cwd: PathBuf::from("/w"),
            argv: vec!["cc".to_owned(), "-O2".to_owned()],
            env: [
                ("KEEP".to_owned(), "1".to_owned()),
                ("GONE".to_owned(), "2".to_owned()),
                ("EDIT".to_owned(), "3".to_owned()),
            ]
            .into(),
            stdin: None,
            capture: Capture::Split,
            stream: false,
            always: false,
            untraced: false,
            ignore: Vec::new(),
            track: Vec::new(),
            timestamps: false,
            machine: Vec::new(),
            expect: None,
            occurrence: 0,
        }
    }

    /// A task record of the program of [`spec`], with module `a` and `b`.
    fn old_task() -> TaskRecord {
        TaskRecord {
            program_hash: [1; 32],
            succeeded: true,
            build: BuildId(1),
            steps: 1,
            line: 1,
            heading: None,
            targets: Vec::new(),
            block: ContentHash([7; 32]),
            modules: vec![
                ModuleHash {
                    path: PathBuf::from("/w/a.star"),
                    hash: ContentHash([2; 32]),
                },
                ModuleHash {
                    path: PathBuf::from("/w/b.star"),
                    hash: ContentHash([3; 32]),
                },
            ],
        }
    }

    #[test]
    fn identical_specifications_have_no_reasons() {
        assert_eq!(
            spec_reasons(&spec(), &spec(), None, None),
            Vec::<Reason>::new()
        );
        let mut repeated = spec();
        repeated.occurrence = 1;
        repeated.stream = true;
        repeated.always = true;
        repeated.expect = Some(vec![0]);
        assert_eq!(
            spec_reasons(&spec(), &repeated, None, None),
            Vec::<Reason>::new()
        );
    }

    #[test]
    fn every_changed_value_gives_its_reason_in_order() {
        let mut new = spec();
        new.program_hash = ContentHash([9; 32]);
        new.argv = vec!["cc".to_owned(), "-O0".to_owned()];
        new.env.remove("GONE");
        new.env.insert("EDIT".to_owned(), "4".to_owned());
        new.env.insert("ADDED".to_owned(), "5".to_owned());
        new.cwd = PathBuf::from("/x");
        new.stdin = Some("in".to_owned());
        new.capture = Capture::Merged;
        new.ignore = vec!["*.o".to_owned()];
        new.track = vec!["/proc/cpuinfo".to_owned()];
        new.timestamps = true;
        new.machine = vec!["kernel".to_owned()];
        let parts = ProgramParts {
            block: [7; 32],
            modules: vec![
                (PathBuf::from("/w/a.star"), [2; 32]),
                (PathBuf::from("/w/b.star"), [4; 32]),
                (PathBuf::from("/w/c.star"), [5; 32]),
            ],
        };
        let reasons = spec_reasons(&spec(), &new, Some(&old_task()), Some(&parts));
        assert_eq!(reasons, vec![
            Reason::ProgramChanged {
                block: false,
                modules: vec![
                    ModuleChange {
                        path: PathBuf::from("/w/b.star"),
                        kind: ModuleChangeKind::Changed,
                    },
                    ModuleChange {
                        path: PathBuf::from("/w/c.star"),
                        kind: ModuleChangeKind::Added,
                    },
                ],
            },
            Reason::ArgumentsChanged {
                old: spec().argv,
                new: new.argv.clone(),
            },
            Reason::EnvironmentChanged {
                changes: vec![
                    EnvChange {
                        name: "ADDED".to_owned(),
                        old: None,
                        new: Some("5".to_owned()),
                    },
                    EnvChange {
                        name: "EDIT".to_owned(),
                        old: Some("3".to_owned()),
                        new: Some("4".to_owned()),
                    },
                    EnvChange {
                        name: "GONE".to_owned(),
                        old: Some("2".to_owned()),
                        new: None,
                    },
                ],
            },
            Reason::WorkingDirectoryChanged {
                old: PathBuf::from("/w"),
                new: PathBuf::from("/x"),
            },
            Reason::StdinChanged {
                old: None,
                new: Some("in".to_owned()),
            },
            Reason::CaptureChanged {
                old: Capture::Split,
                new: Capture::Merged,
            },
            Reason::IgnoreChanged {
                old: Vec::new(),
                new: vec!["*.o".to_owned()],
            },
            Reason::TrackChanged {
                old: Vec::new(),
                new: vec!["/proc/cpuinfo".to_owned()],
            },
            Reason::TimestampsChanged {
                old: false,
                new: true,
            },
            Reason::MachineNamesChanged {
                old: Vec::new(),
                new: vec!["kernel".to_owned()],
            },
        ]);
    }

    #[test]
    fn a_changed_program_names_its_parts_only_when_they_are_known() {
        let mut new = spec();
        new.program_hash = ContentHash([9; 32]);
        let parts = ProgramParts {
            block: [8; 32],
            modules: vec![(PathBuf::from("/w/a.star"), [2; 32])],
        };
        assert_eq!(
            spec_reasons(&spec(), &new, Some(&old_task()), Some(&parts)),
            vec![Reason::ProgramChanged {
                block: true,
                modules: vec![ModuleChange {
                    path: PathBuf::from("/w/b.star"),
                    kind: ModuleChangeKind::Removed,
                }],
            }]
        );
        let unknown = vec![Reason::ProgramChanged {
            block: false,
            modules: Vec::new(),
        }];
        assert_eq!(spec_reasons(&spec(), &new, None, Some(&parts)), unknown);
        let mut other = old_task();
        other.program_hash = [5; 32];
        assert_eq!(
            spec_reasons(&spec(), &new, Some(&other), Some(&parts)),
            unknown
        );
    }

    #[test]
    fn positions_carry_the_self_inflicted_inputs_of_the_previous_build() {
        let input = ChangedInput {
            path: PathBuf::from("/w/cache"),
            change: Change::Content,
            writer: Writer::Outside,
            self_inflicted: Some(SelfInflicted::WritesAndReads),
        };
        let ran = StepPosition {
            step: StepKey([1; 32]),
            build: BuildId(1),
            verdict: Verdict::Ran(Explanation {
                reasons: vec![Reason::InputsChanged {
                    inputs: vec![input],
                }],
                repeated: Vec::new(),
            }),
            carried: vec![PathBuf::from("/w/older")],
        };
        assert_eq!(carried_from(Some(&ran), BuildId(2)), [PathBuf::from(
            "/w/cache"
        )]);
        assert_eq!(carried_from(Some(&ran), BuildId(1)), [PathBuf::from(
            "/w/older"
        )]);
        let cached = StepPosition {
            verdict: Verdict::Cached,
            ..ran
        };
        assert_eq!(
            carried_from(Some(&cached), BuildId(2)),
            Vec::<PathBuf>::new()
        );
        assert_eq!(carried_from(None, BuildId(2)), Vec::<PathBuf>::new());
    }

    #[test]
    fn a_baseline_keeps_the_paths_written_and_read() {
        let observation = |path: &str, access| {
            Observation {
                path: PathBuf::from(path),
                access: AccessBits(access),
                tracked: false,
                state: kizz_store::PathState::Absent,
                attributes: None,
            }
        };
        let baseline = Baseline::new(
            &[
                observation("/both", AccessBits::READ | AccessBits::WRITE),
                observation("/written", AccessBits::WRITE),
                observation("/read", AccessBits::READ),
            ],
            5,
        );
        assert_eq!(baseline.recorded_at_nanos, 5);
        assert_eq!(
            baseline.writes_and_reads,
            [PathBuf::from("/both")].into_iter().collect()
        );
    }
}
