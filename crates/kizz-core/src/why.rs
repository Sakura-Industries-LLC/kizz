// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The report of `kizz why`: the reasons stored by the most recent build for the
//! tasks under a path, with each step's untracked `/proc` and `/sys` accesses, the
//! system calls it made that the tracer does not know, and how many accesses it
//! ignored.
//!
//! The tasks under a directory are the task records whose `kizz.md` is under it; a
//! path that names a file selects the tasks of that file. Among those, the most
//! recent build is the one with the largest build identity, and the report holds
//! the tasks that build ran, each with the step positions of that run. The caller
//! holds the cache lock while it reads, so no build changes the records meanwhile.

use std::{
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
};

use kizz_store::{
    AccessBits, BuildId, PathState, StepKey, StepPosition, StepRecord, Store, StoreError, TaskId,
    TaskLabel, TaskRecord, Verdict, path_prefix,
};

use crate::{explain::RepeatedWarning, render::Render};

/// One step of a task in the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhyStep {
    /// The position of the step in the task's run, counting from zero.
    pub position: u32,
    /// The step key.
    pub key: StepKey,
    /// What the build decided about the step.
    pub verdict: Verdict,
    /// The argument vector, or `None` when the step record is gone.
    pub argv: Option<Vec<String>>,
    /// Every untracked access under `/proc` and `/sys`, sorted by path, with its
    /// kinds.
    pub untracked: Vec<(PathBuf, AccessBits)>,
    /// The number of accesses that ignore patterns left out.
    pub ignored: u64,
    /// The numbers of the system calls that the step's command made and that the
    /// tracer does not know, sorted, as its record lists them.
    pub unknown_system_calls: Vec<u32>,
}

impl WhyStep {
    /// The command, the argument vector joined with spaces, or a note that the
    /// step record is gone.
    #[must_use]
    pub fn command(&self) -> String {
        self.argv.as_ref().map_or_else(
            || "(the step record is no longer in the cache)".to_owned(),
            |argv| argv.join(" "),
        )
    }
}

/// One task of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhyTask {
    /// The task.
    pub id: TaskId,
    /// Its task record.
    pub record: TaskRecord,
    /// The steps of its run in the build, in order.
    pub steps: Vec<WhyStep>,
}

impl WhyTask {
    /// The task as explanations name it.
    #[must_use]
    pub fn label(&self) -> TaskLabel {
        TaskLabel {
            task: self.id.clone(),
            line: self.record.line,
            heading: self.record.heading.clone(),
        }
    }
}

/// The report of `kizz why` for one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhyReport {
    /// The absolute path the report is for.
    pub path: PathBuf,
    /// The most recent build that ran a task under the path, or `None` when no
    /// build did.
    pub build: Option<BuildId>,
    /// The tasks that build ran under the path, in key order: by `kizz.md` path, then
    /// by index.
    pub tasks: Vec<WhyTask>,
}

impl WhyReport {
    /// The loud warning of every step that re-ran in consecutive builds for the same
    /// self-inflicted reason.
    #[must_use]
    pub fn warnings(&self) -> Vec<RepeatedWarning> {
        self.tasks
            .iter()
            .flat_map(|task| {
                task.steps.iter().filter_map(|step| {
                    match &step.verdict {
                        Verdict::Ran(explanation) if !explanation.repeated.is_empty() => {
                            Some(RepeatedWarning {
                                task: Some(task.label()),
                                position: step.position,
                                command: step.command(),
                                paths: explanation.repeated.clone(),
                            })
                        },
                        Verdict::Ran(_) | Verdict::Cached => None,
                    }
                })
            })
            .collect()
    }
}

/// The report of the most recent build for the tasks under `path`, an absolute
/// path, read from `store`.
///
/// # Errors
///
/// A [`StoreError`] when a record cannot be read.
pub fn why(
    store: &Store,
    path: &Path,
) -> Result<WhyReport, StoreError> {
    let txn = store.read()?;
    let prefix = if path.is_file() {
        // The tasks of one `kizz.md`: its path, then the zero byte of task keys.
        let mut prefix = path.as_os_str().as_bytes().to_vec();
        prefix.push(0);
        prefix
    } else {
        path_prefix(path)
    };
    let records = txn.scan::<TaskRecord>(&prefix)?;
    let build = records.iter().map(|(_, record)| record.build).max();
    let mut tasks = Vec::new();
    for (id, record) in records {
        if Some(record.build) != build {
            continue;
        }
        let mut steps = Vec::new();
        for (key, position) in txn.scan::<StepPosition>(&id.key_prefix())? {
            if key.task != id || key.position >= record.steps {
                continue;
            }
            steps.push(step(
                txn.get::<StepRecord>(&position.step)?,
                key.position,
                position,
            ));
        }
        tasks.push(WhyTask { id, record, steps });
    }
    Ok(WhyReport {
        path: path.to_path_buf(),
        build,
        tasks,
    })
}

/// The report of the step at `position`, whose step-position record is `stored`
/// and whose step record is `record`, if it is still there.
fn step(
    record: Option<StepRecord>,
    position: u32,
    stored: StepPosition,
) -> WhyStep {
    let (argv, untracked, ignored, unknown_system_calls) = record.map_or_else(
        || (None, Vec::new(), 0, Vec::new()),
        |record| {
            let untracked = record
                .observations
                .iter()
                .filter(|observation| observation.state == PathState::Untracked)
                .map(|observation| (observation.path.clone(), observation.access))
                .collect();
            (
                Some(record.spec.argv),
                untracked,
                record.ignored,
                record.unknown_system_calls,
            )
        },
    );
    WhyStep {
        position,
        key: stored.step,
        verdict: stored.verdict,
        argv,
        untracked,
        ignored,
        unknown_system_calls,
    }
}

impl Render {
    /// The report of `kizz why` as text: each task with its targets, and each step
    /// with its command, its full reasons, its untracked `/proc` and `/sys` accesses,
    /// on a line of their own the system calls it made that the tracer does not
    /// know, when there are any, and how many accesses it ignored.
    #[must_use]
    pub fn why(
        &self,
        report: &WhyReport,
    ) -> String {
        let path = format!("`{}`", self.path(&report.path));
        if report.tasks.is_empty() {
            return format!("No build has recorded a task under {path}.\n");
        }
        let tasks = if report.tasks.len() == 1 {
            "1 task".to_owned()
        } else {
            format!("{} tasks", report.tasks.len())
        };
        let mut lines = vec![format!("The most recent build ran {tasks} under {path}.")];
        for task in &report.tasks {
            let failed = if task.record.succeeded {
                ""
            } else {
                " (failed)"
            };
            lines.push(String::new());
            lines.push(format!(
                "{} [{}]{failed}",
                self.task(&task.label()),
                task.record.targets.join(", ")
            ));
            if task.steps.is_empty() {
                lines.push("  no commands ran".to_owned());
            }
            for step in &task.steps {
                lines.push(format!(
                    "  step {}: `{}`",
                    step.position.saturating_add(1),
                    step.command()
                ));
                lines.extend(
                    self.full(&step.verdict)
                        .into_iter()
                        .map(|line| format!("    {line}")),
                );
                lines.push(format!("    {}", self.untracked(&step.untracked)));
                if !step.unknown_system_calls.is_empty() {
                    let numbers: Vec<String> = step
                        .unknown_system_calls
                        .iter()
                        .map(ToString::to_string)
                        .collect();
                    lines.push(format!(
                        "    system calls that the tracer does not know: {}",
                        numbers.join(", ")
                    ));
                }
                lines.push(format!("    ignored accesses: {}", step.ignored));
            }
        }
        lines.push(String::new());
        lines.join("\n")
    }

    /// The untracked `/proc` and `/sys` accesses of a step on one line.
    fn untracked(
        &self,
        untracked: &[(PathBuf, AccessBits)],
    ) -> String {
        if untracked.is_empty() {
            return "untracked /proc and /sys accesses: none".to_owned();
        }
        let shown: Vec<String> = untracked
            .iter()
            .map(|(path, access)| {
                format!(
                    "`{}` ({})",
                    self.path(path),
                    crate::render::access_kinds(*access)
                )
            })
            .collect();
        format!("untracked /proc and /sys accesses: {}", shown.join(", "))
    }
}
