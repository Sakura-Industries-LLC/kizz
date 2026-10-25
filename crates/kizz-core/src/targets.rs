// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Target names, target expressions, and task selection.
//!
//! A target expression is one target, or a union of targets joined with `+`, such as
//! `ci+check`. It selects every task tagged with any of its targets, and a task that
//! matches several of them is selected once.

use std::{fmt, str::FromStr};

use thiserror::Error;

use crate::tasks::Task;

/// The rule every target name follows, as error messages state it.
pub const TARGET_NAME_RULE: &str = "target names use only letters, digits, `_`, `.` and `-`";

/// Whether `name` is a valid target name: one or more ASCII letters, digits, `_`,
/// `.` and `-`.
#[must_use]
pub fn is_valid_target(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Why a target expression is malformed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TargetExprError {
    /// The expression, or one of the parts joined with `+`, is empty.
    #[error(
        "invalid target expression `{expression}`: a target is missing; join targets with `+`, \
         such as `ci+check`"
    )]
    MissingTarget {
        /// The whole expression.
        expression: String,
    },
    /// A target in the expression is not a valid target name.
    #[error(
        "invalid target expression `{expression}`: invalid target name `{target}`: {}",
        TARGET_NAME_RULE
    )]
    InvalidTarget {
        /// The whole expression.
        expression: String,
        /// The invalid target.
        target: String,
    },
}

/// A parsed target expression: one or more distinct target names, in the order
/// written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetExpr {
    /// The distinct targets, in the order written.
    targets: Vec<String>,
}

impl TargetExpr {
    /// The distinct targets of the expression, in the order written.
    #[must_use]
    pub fn targets(&self) -> &[String] {
        &self.targets
    }

    /// The tasks the expression selects, in the order of `tasks`, each once, with the
    /// requested targets that selected it.
    #[must_use]
    pub fn select<'tasks>(
        &self,
        tasks: &'tasks [Task],
    ) -> Vec<Selected<'tasks>> {
        tasks
            .iter()
            .filter_map(|task| {
                let selected: Vec<String> = self
                    .targets
                    .iter()
                    .filter(|target| task.targets.contains(target))
                    .cloned()
                    .collect();
                (!selected.is_empty()).then_some(Selected { task, selected })
            })
            .collect()
    }
}

impl FromStr for TargetExpr {
    type Err = TargetExprError;

    fn from_str(expression: &str) -> Result<Self, Self::Err> {
        let mut targets: Vec<String> = Vec::new();
        for target in expression.split('+') {
            if target.is_empty() {
                return Err(TargetExprError::MissingTarget {
                    expression: expression.to_owned(),
                });
            }
            if !is_valid_target(target) {
                return Err(TargetExprError::InvalidTarget {
                    expression: expression.to_owned(),
                    target: target.to_owned(),
                });
            }
            if !targets.iter().any(|known| known == target) {
                targets.push(target.to_owned());
            }
        }
        Ok(Self { targets })
    }
}

impl fmt::Display for TargetExpr {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        f.write_str(&self.targets.join("+"))
    }
}

/// A task that a target expression selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selected<'tasks> {
    /// The task.
    pub task: &'tasks Task,
    /// The requested targets that selected the task, in the order of the expression.
    pub selected: Vec<String>,
}

/// Unit tests for target names, expressions, and selection.
#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use kizz_store::TaskId;

    use super::{TargetExpr, TargetExprError, is_valid_target};
    use crate::tasks::Task;

    /// A task in `kizz.md` at `index`, tagged with `targets`.
    fn task(
        index: u32,
        targets: &[&str],
    ) -> Task {
        Task {
            id: TaskId {
                file: PathBuf::from("/work/kizz.md"),
                index,
            },
            relative_file: PathBuf::from("kizz.md"),
            line: 1,
            heading: None,
            targets: targets.iter().map(|&target| target.to_owned()).collect(),
            source: String::new(),
        }
    }

    /// Target names use letters, digits, `_`, `.` and `-`, and nothing else.
    #[test]
    fn target_names_use_letters_digits_and_three_marks() {
        for name in [
            "ci",
            "check",
            "CI",
            "v1.2",
            "lint_all",
            "pre-commit",
            "0",
            "a.b-c_D9",
        ] {
            assert!(is_valid_target(name), "{name}");
        }
        for name in ["", "ci!", "a b", "a+b", "a/b", "é", "a:b", "*", "a\tb"] {
            assert!(!is_valid_target(name), "{name:?}");
        }
    }

    /// One target and unions parse, keep their order, and drop repeated targets.
    #[test]
    fn expressions_parse_targets_and_unions() {
        let one: TargetExpr = "ci".parse().unwrap();
        assert_eq!(one.targets(), ["ci"]);
        let union: TargetExpr = "ci+check+lint".parse().unwrap();
        assert_eq!(union.targets(), ["ci", "check", "lint"]);
        assert_eq!(union.to_string(), "ci+check+lint");
        let repeated: TargetExpr = "ci+check+ci".parse().unwrap();
        assert_eq!(repeated.targets(), ["ci", "check"]);
    }

    /// Malformed expressions, such as `ci+`, `+`, and `ci!`, are errors that name the
    /// expression.
    #[test]
    fn malformed_expressions_are_errors() {
        for expression in ["", "+", "ci+", "+ci", "ci++check"] {
            assert_eq!(
                expression.parse::<TargetExpr>(),
                Err(TargetExprError::MissingTarget {
                    expression: expression.to_owned()
                }),
                "{expression:?}"
            );
        }
        for (expression, target) in [("ci!", "ci!"), ("ci+a b", "a b"), ("ci+check/x", "check/x")] {
            let error = expression.parse::<TargetExpr>().unwrap_err();
            assert_eq!(error, TargetExprError::InvalidTarget {
                expression: expression.to_owned(),
                target: target.to_owned()
            });
            assert!(error.to_string().contains(target), "{error}");
        }
    }

    /// A union selects every task of any of its targets, in task order, and a task
    /// with several selected targets is selected once, with all of them.
    #[test]
    fn a_union_selects_each_matching_task_once() {
        let tasks = [
            task(0, &["ci", "check"]),
            task(1, &["lint"]),
            task(2, &["check"]),
            task(3, &["docs", "ci"]),
        ];

        let union: TargetExpr = "check+ci".parse().unwrap();
        let selected = union.select(&tasks);
        let indexes: Vec<u32> = selected.iter().map(|s| s.task.id.index).collect();
        assert_eq!(indexes, [0, 2, 3]);
        assert_eq!(selected[0].selected, ["check", "ci"]);
        assert_eq!(selected[1].selected, ["check"]);
        assert_eq!(selected[2].selected, ["ci"]);

        let one: TargetExpr = "lint".parse().unwrap();
        let indexes: Vec<u32> = one.select(&tasks).iter().map(|s| s.task.id.index).collect();
        assert_eq!(indexes, [1]);

        let none: TargetExpr = "release".parse().unwrap();
        assert_eq!(none.select(&tasks), Vec::new());
    }
}
