// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The contents of explanations: why a step ran, or that it was cached, as a build
//! decided it ("Explaining decisions").
//!
//! The step-position records hold a [`Verdict`] for each step of a task's most
//! recent run, so `kizz why` can print the reasons stored by the most recent build.
//! A [`Reason`] is one of the kinds the spec lists:
//!
//! * never run: there is no record for this step;
//! * the task program changed: its block, or a named loaded module;
//! * the arguments changed: old and new, word by word;
//! * the environment changed: the variables added, removed, or changed, with their
//!   values, which only `--verbose` shows;
//! * the working directory, `stdin`, capture mode, ignore patterns, `track` patterns,
//!   `timestamps`, or `machine` changed;
//! * inputs changed: every changed path, what changed about it, and who last wrote it;
//! * the machine state that the step names changed: each changed value, old and new;
//! * the command is marked `always`;
//! * its task is marked `cache=never`;
//! * `kizz run --no-cache` was given;
//! * the previous run could not be cached, and why.
//!
//! Every type here is encoded by the rules of "Step keys": definite lengths, fields
//! in a fixed order, and paths as byte strings.

use std::path::PathBuf;

use minicbor::{Decode, Encode};

use crate::{
    machine::MachineChange,
    records::TaskId,
    step::{Capture, Change, NotCachedReason, path_bytes, path_list},
};

/// The identity of one build: when it started, in nanoseconds since the Unix
/// epoch, made larger than every earlier build of the same process. One process at
/// a time uses a cache, so later builds have larger identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Encode, Decode)]
#[cbor(transparent)]
pub struct BuildId(#[n(0)] pub u64);

/// A task as explanations name it: its identity, the line of its block, and its
/// heading.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct TaskLabel {
    /// The task.
    #[n(0)]
    pub task: TaskId,
    /// The line of the block's opening fence, counting from 1, or 0 when unknown.
    #[n(1)]
    pub line: u32,
    /// The nearest heading above the block, if there is one.
    #[n(2)]
    pub heading: Option<String>,
}

/// Who last wrote a changed input.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub enum Writer {
    /// This command, on its previous run: the step at the same position of the same
    /// task, or a step with the same key.
    #[n(0)]
    ThisCommand,
    /// Another step that ran earlier in this build.
    #[n(1)]
    ThisBuild {
        /// The task of the step.
        #[n(0)]
        task: TaskLabel,
        /// The position of the step in its task's run, counting from zero.
        #[n(1)]
        position: u32,
    },
    /// A step of an earlier build.
    #[n(2)]
    EarlierBuild {
        /// The task of the step.
        #[n(0)]
        task: TaskLabel,
        /// The position of the step in its task's run, counting from zero.
        #[n(1)]
        position: u32,
    },
    /// Something outside kizz: no step or task program recorded a write to the path
    /// after the recorded state was taken.
    #[n(3)]
    Outside,
    /// The program of a task, through `fs.write` or `fs.write_json`, in this build.
    #[n(4)]
    ProgramThisBuild {
        /// The task.
        #[n(0)]
        task: TaskLabel,
    },
    /// The program of a task, through `fs.write` or `fs.write_json`, in an earlier
    /// build.
    #[n(5)]
    ProgramEarlierBuild {
        /// The task.
        #[n(0)]
        task: TaskLabel,
    },
}

/// Why a changed input is a self-inflicted reason to run: kizz's own build changes
/// it on every build.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub enum SelfInflicted {
    /// This command writes the file and also reads it.
    #[n(0)]
    WritesAndReads,
    /// Another step that runs on every build, because it is marked `always`, its
    /// task is marked `cache=never`, or it re-runs for a self-inflicted reason
    /// itself, wrote the file in this build.
    #[n(1)]
    WrittenEveryBuild {
        /// The task of the writing step.
        #[n(0)]
        task: TaskLabel,
        /// The position of the writing step in its task's run, counting from zero.
        #[n(1)]
        position: u32,
    },
}

/// One changed input: the path, what changed about it, and who last wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct ChangedInput {
    /// The absolute path.
    #[cbor(n(0), with = "path_bytes")]
    pub path: PathBuf,
    /// What changed about it.
    #[n(1)]
    pub change: Change,
    /// Who last wrote it.
    #[n(2)]
    pub writer: Writer,
    /// Whether the change is self-inflicted, and how.
    #[n(3)]
    pub self_inflicted: Option<SelfInflicted>,
}

/// How a loaded module changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
#[cbor(index_only)]
pub enum ModuleChangeKind {
    /// Its source changed.
    #[n(0)]
    Changed,
    /// The program loads it now and did not before.
    #[n(1)]
    Added,
    /// The program loaded it before and does not now.
    #[n(2)]
    Removed,
}

/// One loaded module that changed.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct ModuleChange {
    /// The absolute path of the module.
    #[cbor(n(0), with = "path_bytes")]
    pub path: PathBuf,
    /// How it changed.
    #[n(1)]
    pub kind: ModuleChangeKind,
}

/// One environment variable that was added, removed, or changed.
///
/// The values are kept so `--verbose` can show them; without it, output shows only
/// the name, because values can be secrets.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct EnvChange {
    /// The variable's name.
    #[n(0)]
    pub name: String,
    /// The old value, or `None` when the variable was added.
    #[n(1)]
    pub old: Option<String>,
    /// The new value, or `None` when the variable was removed.
    #[n(2)]
    pub new: Option<String>,
}

/// One reason a step ran.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub enum Reason {
    /// There is no record for this step, and no earlier specification at its
    /// position explains why.
    #[n(0)]
    NeverRun,
    /// The task program changed.
    #[n(1)]
    ProgramChanged {
        /// Whether the block changed.
        #[n(0)]
        block: bool,
        /// Every loaded module that changed, was added, or was removed, sorted by
        /// path. With neither a block nor a module, which part changed is unknown.
        #[n(1)]
        modules: Vec<ModuleChange>,
    },
    /// The arguments changed.
    #[n(2)]
    ArgumentsChanged {
        /// The old argument vector.
        #[n(0)]
        old: Vec<String>,
        /// The new argument vector.
        #[n(1)]
        new: Vec<String>,
    },
    /// The environment changed.
    #[n(3)]
    EnvironmentChanged {
        /// Every variable added, removed, or changed, sorted by name.
        #[n(0)]
        changes: Vec<EnvChange>,
    },
    /// The working directory changed.
    #[n(4)]
    WorkingDirectoryChanged {
        /// The old working directory.
        #[cbor(n(0), with = "path_bytes")]
        old: PathBuf,
        /// The new working directory.
        #[cbor(n(1), with = "path_bytes")]
        new: PathBuf,
    },
    /// The `stdin` text changed.
    #[n(5)]
    StdinChanged {
        /// The old text, or `None` for `/dev/null`.
        #[n(0)]
        old: Option<String>,
        /// The new text, or `None` for `/dev/null`.
        #[n(1)]
        new: Option<String>,
    },
    /// The capture mode changed.
    #[n(6)]
    CaptureChanged {
        /// The old mode.
        #[n(0)]
        old: Capture,
        /// The new mode.
        #[n(1)]
        new: Capture,
    },
    /// The ignore patterns changed.
    #[n(7)]
    IgnoreChanged {
        /// The old patterns.
        #[n(0)]
        old: Vec<String>,
        /// The new patterns.
        #[n(1)]
        new: Vec<String>,
    },
    /// The `track` patterns changed.
    #[n(8)]
    TrackChanged {
        /// The old patterns.
        #[n(0)]
        old: Vec<String>,
        /// The new patterns.
        #[n(1)]
        new: Vec<String>,
    },
    /// The `timestamps` option changed.
    #[n(12)]
    TimestampsChanged {
        /// The old value.
        #[n(0)]
        old: bool,
        /// The new value.
        #[n(1)]
        new: bool,
    },
    /// The `machine` names changed.
    #[n(13)]
    MachineNamesChanged {
        /// The old names.
        #[n(0)]
        old: Vec<String>,
        /// The new names.
        #[n(1)]
        new: Vec<String>,
    },
    /// Recorded inputs changed.
    #[n(9)]
    InputsChanged {
        /// Every changed input, sorted by path.
        #[n(0)]
        inputs: Vec<ChangedInput>,
    },
    /// Values of the machine state that the step names changed.
    #[n(14)]
    MachineChanged {
        /// Each changed value, old and new, in the order of the step's names.
        #[n(0)]
        changes: Vec<MachineChange>,
    },
    /// The command is marked `always`.
    #[n(10)]
    Always,
    /// The previous run could not be cached.
    #[n(11)]
    NotCached {
        /// Why it could not be cached.
        #[n(0)]
        why: NotCachedReason,
        /// Every input that changed since that run, sorted by path.
        #[n(1)]
        inputs: Vec<ChangedInput>,
    },
    /// The command's task is marked `cache=never`, so the command runs as if it were
    /// marked `always`.
    #[n(15)]
    TaskCacheNever,
    /// `kizz run --no-cache` was given, so the command ran although its record may
    /// have been a cache hit. Unlike [`Reason::Always`] and
    /// [`Reason::TaskCacheNever`], it does not make the step one that runs on every
    /// build.
    #[n(16)]
    RunNoCache,
    /// Dependency tracing is disabled; the command runs once per build.
    #[n(17)]
    Untraced,
    /// The dependency tracing mode changed.
    #[n(18)]
    TracingChanged {
        /// The previous value of the untraced option.
        #[n(0)]
        old: bool,
        /// The current value of the untraced option.
        #[n(1)]
        new: bool,
    },
}

impl Reason {
    /// The changed inputs of this reason, if it has any.
    #[must_use]
    pub fn inputs(&self) -> &[ChangedInput] {
        match self {
            Self::InputsChanged { inputs } | Self::NotCached { inputs, .. } => inputs,
            _ => &[],
        }
    }
}

/// Why a step ran.
#[derive(Debug, Clone, PartialEq, Eq, Default, Encode, Decode)]
pub struct Explanation {
    /// Every reason, in the order of the spec's list.
    #[n(0)]
    pub reasons: Vec<Reason>,
    /// The self-inflicted inputs that also made the step run in its task's previous
    /// build, sorted: the step re-runs in consecutive builds for the same
    /// self-inflicted reason, which deserves a loud warning.
    #[cbor(n(1), with = "path_list")]
    pub repeated: Vec<PathBuf>,
}

impl Explanation {
    /// The paths of every self-inflicted changed input, sorted and without
    /// duplicates.
    #[must_use]
    pub fn self_inflicted(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self
            .reasons
            .iter()
            .flat_map(Reason::inputs)
            .filter(|input| input.self_inflicted.is_some())
            .map(|input| input.path.clone())
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    /// Whether the step runs on every build: it is marked `always`, its task is
    /// marked `cache=never`, or one of its changed inputs is self-inflicted.
    #[must_use]
    pub fn runs_every_build(&self) -> bool {
        self.reasons.iter().any(|reason| {
            matches!(
                reason,
                Reason::Always | Reason::TaskCacheNever | Reason::Untraced
            ) || reason
                .inputs()
                .iter()
                .any(|input| input.self_inflicted.is_some())
        })
    }
}

/// What a build decided about one step.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub enum Verdict {
    /// The step ran, for these reasons.
    #[n(0)]
    Ran(#[n(0)] Explanation),
    /// The step was cached: every recorded input was unchanged, or its result in
    /// this build was returned again.
    #[n(1)]
    Cached,
}

/// Unit tests for the encoding of explanations.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]
mod tests {
    use super::*;

    /// A task label.
    fn label() -> TaskLabel {
        TaskLabel {
            task: TaskId {
                file: PathBuf::from("/w/kizz.md"),
                index: 1,
            },
            line: 3,
            heading: Some("Build".to_owned()),
        }
    }

    /// One reason of each kind.
    fn reasons() -> Vec<Reason> {
        let input = |writer, self_inflicted| {
            ChangedInput {
                path: PathBuf::from("/w/in\u{e9}"),
                change: Change::Content,
                writer,
                self_inflicted,
            }
        };
        vec![
            Reason::NeverRun,
            Reason::ProgramChanged {
                block: true,
                modules: vec![ModuleChange {
                    path: PathBuf::from("/w/lib.star"),
                    kind: ModuleChangeKind::Removed,
                }],
            },
            Reason::ArgumentsChanged {
                old: vec!["a".to_owned()],
                new: vec!["b".to_owned()],
            },
            Reason::EnvironmentChanged {
                changes: vec![EnvChange {
                    name: "A".to_owned(),
                    old: None,
                    new: Some("1".to_owned()),
                }],
            },
            Reason::WorkingDirectoryChanged {
                old: PathBuf::from("/a"),
                new: PathBuf::from("/b"),
            },
            Reason::StdinChanged {
                old: None,
                new: Some("x".to_owned()),
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
                old: vec!["/proc/x".to_owned()],
                new: Vec::new(),
            },
            Reason::TimestampsChanged {
                old: false,
                new: true,
            },
            Reason::MachineNamesChanged {
                old: Vec::new(),
                new: vec!["kernel".to_owned()],
            },
            Reason::InputsChanged {
                inputs: vec![
                    input(Writer::ThisCommand, Some(SelfInflicted::WritesAndReads)),
                    input(
                        Writer::ThisBuild {
                            task: label(),
                            position: 2,
                        },
                        Some(SelfInflicted::WrittenEveryBuild {
                            task: label(),
                            position: 2,
                        }),
                    ),
                    input(
                        Writer::EarlierBuild {
                            task: label(),
                            position: 0,
                        },
                        None,
                    ),
                    input(Writer::Outside, None),
                    input(Writer::ProgramThisBuild { task: label() }, None),
                    input(Writer::ProgramEarlierBuild { task: label() }, None),
                ],
            },
            Reason::MachineChanged {
                changes: vec![MachineChange {
                    value: "kernel release".to_owned(),
                    old: "6.12.0".to_owned(),
                    new: "6.13.0".to_owned(),
                }],
            },
            Reason::Always,
            Reason::TaskCacheNever,
            Reason::RunNoCache,
            Reason::NotCached {
                why: NotCachedReason::Signaled { signal: 9 },
                inputs: Vec::new(),
            },
        ]
    }

    #[test]
    fn verdicts_round_trip_with_stable_bytes() {
        let verdicts = [
            Verdict::Cached,
            Verdict::Ran(Explanation {
                reasons: reasons(),
                repeated: vec![PathBuf::from("/w/in\u{e9}"), PathBuf::from("/w/b")],
            }),
        ];
        for verdict in verdicts {
            let first = minicbor::to_vec(&verdict).unwrap();
            assert_eq!(first, minicbor::to_vec(&verdict).unwrap());
            assert_eq!(minicbor::decode::<Verdict>(&first).unwrap(), verdict);
        }
    }

    #[test]
    fn self_inflicted_paths_and_every_build_runs() {
        let explanation = Explanation {
            reasons: reasons(),
            repeated: Vec::new(),
        };
        assert_eq!(explanation.self_inflicted(), [PathBuf::from("/w/in\u{e9}")]);
        assert!(explanation.runs_every_build());
        let plain = Explanation {
            reasons: vec![Reason::NeverRun],
            repeated: Vec::new(),
        };
        assert_eq!(plain.self_inflicted(), Vec::<PathBuf>::new());
        assert!(!plain.runs_every_build());
        let always = Explanation {
            reasons: vec![Reason::Always],
            repeated: Vec::new(),
        };
        assert!(always.runs_every_build());
        let never = Explanation {
            reasons: vec![Reason::TaskCacheNever],
            repeated: Vec::new(),
        };
        assert!(never.runs_every_build());
        assert_eq!(never.self_inflicted(), Vec::<PathBuf>::new());
        let no_cache = Explanation {
            reasons: vec![Reason::RunNoCache],
            repeated: Vec::new(),
        };
        assert!(!no_cache.runs_every_build());
        assert_eq!(no_cache.self_inflicted(), Vec::<PathBuf>::new());
    }

    /// The reason of a command whose task is marked `cache=never` encodes
    /// differently from `always`, and an explanation that holds it round-trips.
    #[test]
    fn the_task_cache_never_reason_encodes_and_round_trips() {
        let never = minicbor::to_vec(&Reason::TaskCacheNever).unwrap();
        assert_ne!(never, minicbor::to_vec(&Reason::Always).unwrap());
        assert_eq!(
            minicbor::decode::<Reason>(&never).unwrap(),
            Reason::TaskCacheNever
        );
        let verdict = Verdict::Ran(Explanation {
            reasons: vec![Reason::TaskCacheNever],
            repeated: Vec::new(),
        });
        let bytes = minicbor::to_vec(&verdict).unwrap();
        assert_eq!(minicbor::decode::<Verdict>(&bytes).unwrap(), verdict);
    }

    /// The reason of a command that ran because `kizz run --no-cache` was given
    /// encodes differently from `always` and from `cache=never`, and an explanation
    /// that holds it round-trips.
    #[test]
    fn the_run_no_cache_reason_encodes_and_round_trips() {
        let no_cache = minicbor::to_vec(&Reason::RunNoCache).unwrap();
        assert_ne!(no_cache, minicbor::to_vec(&Reason::Always).unwrap());
        assert_ne!(no_cache, minicbor::to_vec(&Reason::TaskCacheNever).unwrap());
        assert_eq!(
            minicbor::decode::<Reason>(&no_cache).unwrap(),
            Reason::RunNoCache
        );
        let verdict = Verdict::Ran(Explanation {
            reasons: vec![Reason::RunNoCache],
            repeated: Vec::new(),
        });
        let bytes = minicbor::to_vec(&verdict).unwrap();
        assert_eq!(minicbor::decode::<Verdict>(&bytes).unwrap(), verdict);
    }
}
