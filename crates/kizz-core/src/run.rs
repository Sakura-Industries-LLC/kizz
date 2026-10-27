// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Running one task by its name, for `kizz run` ("Running one task with
//! `kizz run`").
//!
//! [`select_by_name`] finds the task named by `kizz run <name>` among the tasks
//! discovered under the current directory: the task whose `kizz.md` is the fewest
//! directory levels below the current directory wins ([`task_level`]), two tasks of
//! that name at the same level are an error that lists each with its path, and a
//! name that no task has is an error that lists the task names under the current
//! directory. A task's level is counted along the path that discovery reports its
//! `kizz.md` under, the first path of a file reached through several.
//!
//! [`run`] runs the selected task once, with its arguments in `task.args` and an
//! empty `task.selected` ([`TaskInfo::for_run`]): no other task runs, and no pass
//! follows, even when its program changes a file. It uses and updates the cache as
//! a build does, and stores what a build of that one task in one pass stores. The
//! engine options say whether every command runs (`--no-cache`, see
//! [`Engine::with_no_cache`]) and echoes its output live
//! ([`Engine::with_echo_every_step`]), and the [`StepEvents`] follow each step as it
//! starts or comes from the cache.
//!
//! With [`Engine::with_report`] (`kizz run --output`), the run also keeps what its
//! observation report needs ([`RunObservations`]): the command of each step that
//! the run reached, in order, a step that ran with its process tree and a cache hit
//! as cached, and the task program's own file accesses, its module reads included,
//! with their outcomes, which the tracer's error-name query names.
//! [`RunReport::observation_report`] builds the report from them, each command the
//! root of its own process tree and the task program an entry of its own. Without
//! the option, nothing more is kept.

use std::{collections::BTreeMap, path::Component};

use kizz_store::{StoreError, Verdict};
use kizz_trace::Observations;
use thiserror::Error;

use crate::{
    build::{TaskResult, UncheckedStep, run_task_once},
    engine::{Engine, StepEvents},
    report::{CommandInput, Report, TaskProgramInput},
    runtime::{PrintSink, TaskInfo},
    targets::is_valid_target,
    tasks::{TASK_NAME_RULE, Task},
};

/// Why `kizz run <name>` selects no task.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NameError {
    /// The name is not a valid task name.
    #[error("invalid task name `{name}`: {}", TASK_NAME_RULE)]
    Invalid {
        /// The name as given.
        name: String,
    },
    /// No task under the current directory has the name.
    #[error("no task named `{name}` under `.`; {}", names_clause(.names))]
    Unknown {
        /// The name as given.
        name: String,
        /// The task names under the current directory, sorted and without repeats.
        names: Vec<String>,
    },
    /// Several tasks of the name are the fewest directory levels below the current
    /// directory.
    #[error(
        "{} tasks named `{name}` are {} below `.`, so kizz cannot choose one: {}",
        .tasks.len(),
        levels(*.level),
        .tasks.join(", ")
    )]
    Tie {
        /// The name as given.
        name: String,
        /// The number of directory levels between the current directory and their
        /// `kizz.md` files.
        level: usize,
        /// Each task, as `path/kizz.md:LINE (Heading)`, in the default order.
        tasks: Vec<String>,
    },
}

/// `level` directory levels, as messages say it.
fn levels(level: usize) -> String {
    if level == 1 {
        "1 directory level".to_owned()
    } else {
        format!("{level} directory levels")
    }
}

/// The clause of [`NameError::Unknown`] that lists the task names `names`.
fn names_clause(names: &[String]) -> String {
    if names.is_empty() {
        "there is no named task under `.`".to_owned()
    } else {
        format!("the task names under `.` are: {}", names.join(", "))
    }
}

/// The number of directory levels between the directory discovery started in and
/// the `kizz.md` of `task`: 0 for a `kizz.md` in that directory, 1 for one in a
/// subdirectory, and so on, counted along the path that discovery reports it under.
#[must_use]
pub fn task_level(task: &Task) -> usize {
    task.relative_file.parent().map_or(0, |dir| {
        dir.components()
            .filter(|component| matches!(component, Component::Normal(_)))
            .count()
    })
}

/// The task of `tasks`, the tasks discovered under one directory in the default
/// order, that `kizz run <name>` runs: the task named `name` whose `kizz.md` is the
/// fewest directory levels below that directory.
///
/// # Errors
///
/// A [`NameError`] when `name` is not a valid task name, when no task has it, or
/// when several tasks have it at the fewest levels.
pub fn select_by_name<'t>(
    tasks: &'t [Task],
    name: &str,
) -> Result<&'t Task, NameError> {
    if !is_valid_target(name) {
        return Err(NameError::Invalid {
            name: name.to_owned(),
        });
    }
    let matching: Vec<&Task> = tasks
        .iter()
        .filter(|task| task.options.name.as_deref() == Some(name))
        .collect();
    let Some(level) = matching.iter().map(|task| task_level(task)).min() else {
        let mut names: Vec<String> = tasks
            .iter()
            .filter_map(|task| task.options.name.clone())
            .collect();
        names.sort();
        names.dedup();
        return Err(NameError::Unknown {
            name: name.to_owned(),
            names,
        });
    };
    let nearest: Vec<&Task> = matching
        .into_iter()
        .filter(|task| task_level(task) == level)
        .collect();
    match nearest.as_slice() {
        [task] => Ok(*task),
        _ => {
            Err(NameError::Tie {
                name: name.to_owned(),
                level,
                tasks: nearest.iter().map(|task| task.location()).collect(),
            })
        },
    }
}

/// What one `kizz run` observed for its observation report, which it keeps only
/// when the engine writes one ([`Engine::with_report`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunObservations {
    /// The command of each step that the run reached, in order: a step that ran,
    /// or whose program kizz did not find, with its outcome, its process tree and
    /// the lookup's observations, and a cache hit as cached.
    pub commands: Vec<CommandInput>,
    /// The task program's own file accesses, its module reads and its `fs` and
    /// `env.load_file` accesses, each with its outcomes.
    pub task_program: Observations,
}

/// What one `kizz run` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    /// The one run of the task.
    pub result: TaskResult,
    /// Whether cancellation was requested, so the run started nothing new.
    pub interrupted: bool,
    /// What the run observed for its observation report, when the engine writes
    /// one, and otherwise `None`.
    pub observed: Option<RunObservations>,
}

impl RunReport {
    /// Whether the run succeeded: it was not interrupted, and the task did not
    /// fail.
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        !self.interrupted && self.result.result.is_ok()
    }

    /// The steps that the run left unchecked, which warn once the run ends; none
    /// when it was interrupted, as for a build.
    #[must_use]
    pub fn unchecked(&self) -> Vec<UncheckedStep> {
        if self.interrupted {
            Vec::new()
        } else {
            self.result.unchecked.clone()
        }
    }

    /// The number of commands that ran, and the number that came from the cache.
    #[must_use]
    pub fn command_counts(&self) -> (usize, usize) {
        let ran = self
            .result
            .decisions
            .iter()
            .filter(|decision| matches!(decision.verdict, Verdict::Ran(_)))
            .count();
        (ran, self.result.decisions.len().saturating_sub(ran))
    }

    /// The observation report of the run, whose task program is named `task`, as
    /// output names the task, such as `kizz.md:3 (Build)`: one command for each
    /// step that the run reached, in order, each that ran the root of its own
    /// process tree and each cache hit cached, without observations, and the task
    /// program's own accesses as an entry of their own. `None` when the engine
    /// wrote no report.
    #[must_use]
    pub fn observation_report(
        &self,
        task: &str,
    ) -> Option<Report> {
        self.observed.as_ref().map(|observed| {
            Report::new(
                observed.commands.clone(),
                Some(TaskProgramInput {
                    task: task.to_owned(),
                    observations: observed.task_program.clone(),
                }),
            )
        })
    }
}

/// Run `task`, made with [`TaskInfo::for_run`], once with `engine`, as `kizz run`
/// does; `caller` is the caller environment, the lines its program prints go to
/// `print`, and `events` follows each step as it starts or comes from the cache.
///
/// # Errors
///
/// A [`StoreError`] when a record cannot be read or written.
pub fn run(
    engine: &Engine<'_>,
    task: &TaskInfo,
    caller: &BTreeMap<String, String>,
    print: &dyn PrintSink,
    events: &dyn StepEvents,
) -> Result<RunReport, StoreError> {
    let (result, observed) = run_task_once(engine, task, caller, print, events)?;
    Ok(RunReport {
        result,
        interrupted: engine.is_cancelled(),
        observed,
    })
}

/// Unit tests for the levels of tasks and the messages of selection.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]
mod tests {
    use std::path::PathBuf;

    use kizz_store::TaskId;

    use super::{NameError, select_by_name, task_level};
    use crate::tasks::{Task, TaskOptions};

    /// A task named `name` in the `kizz.md` at `relative`, the `index`th of the
    /// project.
    fn task(
        relative: &str,
        index: u32,
        name: Option<&str>,
    ) -> Task {
        Task {
            id: TaskId {
                file: PathBuf::from("/work").join(relative),
                index,
            },
            relative_file: PathBuf::from(relative),
            line: 3,
            heading: Some("Heading".to_owned()),
            targets: Vec::new(),
            options: TaskOptions {
                name: name.map(str::to_owned),
                cache_never: false,
            },
            source: String::new(),
        }
    }

    /// A `kizz.md` in the directory is at level 0, and each directory below adds
    /// one.
    #[test]
    fn levels_count_the_directories_below() {
        assert_eq!(task_level(&task("kizz.md", 0, None)), 0);
        assert_eq!(task_level(&task("a/kizz.md", 0, None)), 1);
        assert_eq!(task_level(&task("a/b/c/kizz.md", 0, None)), 3);
    }

    /// The nearest task wins, a tie names both tasks with their paths, and an
    /// unknown or invalid name says so.
    #[test]
    fn the_nearest_task_wins_and_ties_and_unknown_names_fail() {
        let tasks = [
            task("kizz.md", 0, Some("other")),
            task("a/kizz.md", 0, Some("lint")),
            task("b/kizz.md", 0, Some("lint")),
            task("c/d/kizz.md", 0, Some("deep")),
            task("e/kizz.md", 0, Some("deep")),
        ];
        let deep = select_by_name(&tasks, "deep").unwrap();
        assert_eq!(deep.relative_file, PathBuf::from("e/kizz.md"));
        let tie = select_by_name(&tasks, "lint").unwrap_err();
        assert_eq!(tie, NameError::Tie {
            name: "lint".to_owned(),
            level: 1,
            tasks: vec![
                "a/kizz.md:3 (Heading)".to_owned(),
                "b/kizz.md:3 (Heading)".to_owned()
            ],
        });
        assert_eq!(
            tie.to_string(),
            "2 tasks named `lint` are 1 directory level below `.`, so kizz cannot choose one: \
             a/kizz.md:3 (Heading), b/kizz.md:3 (Heading)"
        );
        let unknown = select_by_name(&tasks, "build").unwrap_err();
        assert_eq!(
            unknown.to_string(),
            "no task named `build` under `.`; the task names under `.` are: deep, lint, other"
        );
        let none = select_by_name(&[task("kizz.md", 0, None)], "build").unwrap_err();
        assert_eq!(
            none.to_string(),
            "no task named `build` under `.`; there is no named task under `.`"
        );
        let invalid = select_by_name(&tasks, "a b").unwrap_err();
        assert_eq!(
            invalid.to_string(),
            "invalid task name `a b`: task names use only letters, digits, `_`, `.` and `-`"
        );
    }
}
