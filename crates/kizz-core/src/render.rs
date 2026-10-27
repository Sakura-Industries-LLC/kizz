// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! How explanations read: the one-line reason that `kizz build` prints for each
//! command it runs, the full reasons of `--explain` and `kizz why`, and the loud
//! warning for a step that re-runs in consecutive builds for the same
//! self-inflicted reason.
//!
//! Output is plain text; the command line adds styling. Paths under the base
//! directory are shown relative to it, paths under the home directory start with
//! `~/`, and other paths are absolute. Environment values, which can be secrets,
//! are shown only when the output is verbose, and so is `stdin` text.

use std::path::{Path, PathBuf};

use kizz_store::{
    AccessBits, Capture, ChangedInput, EnvChange, Explanation, ModuleChange, ModuleChangeKind,
    NotCachedReason, Reason, SelfInflicted, TaskLabel, Verdict, Writer, changed_reads_text,
    machine_changes_text,
};

use crate::{explain::RepeatedWarning, warning::location};

/// The one-line reason of a step that was cached.
pub const CACHED: &str = "cached: no recorded input changed";

/// How explanations are rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Render {
    /// Paths under this directory are shown relative to it.
    pub base: PathBuf,
    /// Paths under this directory, the caller's home directory, start with `~/`.
    pub home: Option<PathBuf>,
    /// Whether environment values and `stdin` text are shown.
    pub verbose: bool,
}

/// `text` in backticks.
fn quoted(text: &str) -> String {
    format!("`{text}`")
}

/// A list of patterns, each in backticks, or `none`.
fn patterns(list: &[String]) -> String {
    if list.is_empty() {
        "none".to_owned()
    } else {
        list.iter()
            .map(|pattern| quoted(pattern))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// A capture mode as output names it.
const fn capture(mode: Capture) -> &'static str {
    match mode {
        Capture::Split => "split",
        Capture::Merged => "merged",
    }
}

/// A boolean as a task program writes it: `True` or `False`.
const fn starlark_bool(value: bool) -> &'static str {
    if value { "True" } else { "False" }
}

/// `stdin` text as verbose output shows it, or `/dev/null` for none.
fn stdin_text(text: Option<&str>) -> String {
    text.map_or_else(|| "/dev/null".to_owned(), |text| format!("{text:?}"))
}

/// `number` followed by `singular`, made plural with `s` unless `number` is 1.
fn count(
    number: usize,
    singular: &str,
) -> String {
    if number == 1 {
        format!("1 {singular}")
    } else {
        format!("{number} {singular}s")
    }
}

/// The access kinds `bits` as output names them, such as `read, probe`.
#[must_use]
pub fn access_kinds(bits: AccessBits) -> String {
    [
        (AccessBits::READ, "read"),
        (AccessBits::PROBE, "probe"),
        (AccessBits::LIST, "list"),
        (AccessBits::WRITE, "write"),
        (AccessBits::EXEC, "exec"),
        (AccessBits::XATTR, "xattr"),
    ]
    .into_iter()
    .filter(|(bit, _)| bits.contains(*bit))
    .map(|(_, name)| name)
    .collect::<Vec<_>>()
    .join(", ")
}

impl Render {
    /// A path as output shows it.
    #[must_use]
    pub fn path(
        &self,
        path: &Path,
    ) -> String {
        if let Ok(relative) = path.strip_prefix(&self.base) {
            return if relative.as_os_str().is_empty() {
                ".".to_owned()
            } else {
                relative.display().to_string()
            };
        }
        if let Some(rest) = self
            .home
            .as_ref()
            .and_then(|home| path.strip_prefix(home).ok())
        {
            return format!("~/{}", rest.display());
        }
        path.display().to_string()
    }

    /// A path in backticks.
    fn quoted_path(
        &self,
        path: &Path,
    ) -> String {
        quoted(&self.path(path))
    }

    /// Why a run could not be cached, with the files that changed after the command
    /// read them shown as other paths are, each with who changed it.
    #[must_use]
    pub fn not_cached(
        &self,
        why: &NotCachedReason,
    ) -> String {
        if let NotCachedReason::ChangedAfterRead { files } = why {
            changed_reads_text(files, |path| self.quoted_path(path))
        } else {
            why.to_string()
        }
    }

    /// A task as output names it: `path/kizz.md:LINE (Heading)`, or the path alone
    /// when the line is unknown.
    #[must_use]
    pub fn task(
        &self,
        label: &TaskLabel,
    ) -> String {
        let file = PathBuf::from(self.path(&label.task.file));
        if label.line == 0 {
            return file.display().to_string();
        }
        location(
            &file,
            usize::try_from(label.line).unwrap_or(usize::MAX),
            label.heading.as_deref(),
        )
    }

    /// A step of a task, such as `step 2 of kizz.md:3 (Build)`.
    fn step_of(
        &self,
        task: &TaskLabel,
        position: u32,
    ) -> String {
        format!("step {} of {}", position.saturating_add(1), self.task(task))
    }

    /// Who last wrote a changed input.
    #[must_use]
    pub fn writer(
        &self,
        writer: &Writer,
    ) -> String {
        match writer {
            Writer::ThisCommand => "this command on its previous run".to_owned(),
            Writer::ThisBuild { task, position } => {
                format!("{} in this build", self.step_of(task, *position))
            },
            Writer::EarlierBuild { task, position } => {
                format!("{} in an earlier build", self.step_of(task, *position))
            },
            Writer::Outside => "something outside kizz".to_owned(),
            Writer::ProgramThisBuild { task } => {
                format!("the program of {} in this build", self.task(task))
            },
            Writer::ProgramEarlierBuild { task } => {
                format!("the program of {} in an earlier build", self.task(task))
            },
        }
    }

    /// Why a changed input is self-inflicted.
    fn self_inflicted(
        &self,
        how: &SelfInflicted,
    ) -> String {
        match how {
            SelfInflicted::WritesAndReads => {
                "this command writes that file and also reads it".to_owned()
            },
            SelfInflicted::WrittenEveryBuild { task, position } => {
                format!(
                    "{} writes that file on every build",
                    self.step_of(task, *position)
                )
            },
        }
    }

    /// The direct explanation of a self-inflicted input, such as "re-ran because
    /// `out` changed; this command writes that file and also reads it".
    fn direct(
        &self,
        input: &ChangedInput,
        how: &SelfInflicted,
    ) -> String {
        format!(
            "re-ran because {} changed; {}",
            self.quoted_path(&input.path),
            self.self_inflicted(how)
        )
    }

    /// One changed input on one line: the path, what changed, and who last wrote
    /// it.
    fn input(
        &self,
        input: &ChangedInput,
    ) -> String {
        format!(
            "{}: {}; last written by {}",
            self.quoted_path(&input.path),
            input.change,
            self.writer(&input.writer)
        )
    }

    /// Changed inputs in short: the first self-inflicted one directly, or else the
    /// first one, and how many more there are.
    fn inputs_short(
        &self,
        inputs: &[ChangedInput],
    ) -> String {
        let first = inputs
            .iter()
            .find_map(|input| {
                input
                    .self_inflicted
                    .as_ref()
                    .map(|how| self.direct(input, how))
            })
            .or_else(|| {
                inputs.first().map(|input| {
                    format!(
                        "{} changed ({})",
                        self.quoted_path(&input.path),
                        input.change
                    )
                })
            });
        let Some(first) = first else {
            return "no input changed".to_owned();
        };
        match inputs.len().saturating_sub(1) {
            0 => first,
            more => format!("{first}, and {} changed", count(more, "more input")),
        }
    }

    /// Changed inputs in full: one line each, and the direct explanation of each
    /// self-inflicted one.
    fn inputs_full(
        &self,
        inputs: &[ChangedInput],
    ) -> Vec<String> {
        let mut lines = Vec::new();
        for input in inputs {
            lines.push(format!("  {}", self.input(input)));
            if let Some(how) = &input.self_inflicted {
                lines.push(format!("    {}", self.direct(input, how)));
            }
        }
        lines
    }

    /// A changed program, naming its parts when they are known.
    fn program(
        &self,
        block: bool,
        modules: &[ModuleChange],
    ) -> String {
        let mut parts = Vec::new();
        if block {
            parts.push("its block".to_owned());
        }
        for module in modules {
            let path = self.quoted_path(&module.path);
            parts.push(match module.kind {
                ModuleChangeKind::Changed => format!("module {path}"),
                ModuleChangeKind::Added => format!("module {path}, which it now loads"),
                ModuleChangeKind::Removed => format!("module {path}, which it no longer loads"),
            });
        }
        if parts.is_empty() {
            "the task program changed".to_owned()
        } else {
            format!("the task program changed: {}", parts.join(", "))
        }
    }

    /// One changed environment variable, with its values only when verbose.
    fn env_change(
        &self,
        change: &EnvChange,
    ) -> String {
        let name = &change.name;
        match (&change.old, &change.new, self.verbose) {
            (None, Some(new), true) => format!("added {name}={new}"),
            (Some(old), None, true) => format!("removed {name}={old}"),
            (Some(old), Some(new), true) => {
                format!("changed {name} from {} to {}", quoted(old), quoted(new))
            },
            (None, ..) => format!("added {name}"),
            (Some(_), None, _) => format!("removed {name}"),
            (Some(_), Some(_), _) => format!("changed {name}"),
        }
    }

    /// The arguments, word by word: each word that changed, was added, or was
    /// removed, counting the program as word 1.
    fn words(
        old: &[String],
        new: &[String],
    ) -> Vec<String> {
        let len = old.len().max(new.len());
        (0..len)
            .filter_map(|index| {
                let number = index.saturating_add(1);
                match (old.get(index), new.get(index)) {
                    (Some(old), Some(new)) if old == new => None,
                    (Some(old), Some(new)) => {
                        Some(format!(
                            "word {number}: {} became {}",
                            quoted(old),
                            quoted(new)
                        ))
                    },
                    (None, Some(new)) => Some(format!("word {number} added: {}", quoted(new))),
                    (Some(old), None) => Some(format!("word {number} removed: {}", quoted(old))),
                    (None, None) => None,
                }
            })
            .collect()
    }

    /// The short form of `stdin` changing.
    fn stdin(
        &self,
        old: Option<&str>,
        new: Option<&str>,
    ) -> String {
        if self.verbose {
            format!(
                "`stdin` changed from {} to {}",
                stdin_text(old),
                stdin_text(new)
            )
        } else {
            "`stdin` changed".to_owned()
        }
    }

    /// One reason on one line.
    #[must_use]
    pub fn reason_line(
        &self,
        reason: &Reason,
    ) -> String {
        match reason {
            Reason::NeverRun => "never run: there is no record for this step".to_owned(),
            Reason::ProgramChanged { block, modules } => self.program(*block, modules),
            Reason::ArgumentsChanged { old, new } => {
                let words = Self::words(old, new);
                format!("the arguments changed: {}", words.join(", "))
            },
            Reason::EnvironmentChanged { changes } => {
                let changes: Vec<String> = changes
                    .iter()
                    .map(|change| self.env_change(change))
                    .collect();
                format!("the environment changed: {}", changes.join(", "))
            },
            Reason::WorkingDirectoryChanged { old, new } => {
                format!(
                    "the working directory changed from {} to {}",
                    self.quoted_path(old),
                    self.quoted_path(new)
                )
            },
            Reason::StdinChanged { old, new } => self.stdin(old.as_deref(), new.as_deref()),
            Reason::CaptureChanged { old, new } => {
                format!(
                    "the capture mode changed from {} to {}",
                    capture(*old),
                    capture(*new)
                )
            },
            Reason::IgnoreChanged { old, new } => {
                format!(
                    "the ignore patterns changed from {} to {}",
                    patterns(old),
                    patterns(new)
                )
            },
            Reason::TrackChanged { old, new } => {
                format!(
                    "the `track` patterns changed from {} to {}",
                    patterns(old),
                    patterns(new)
                )
            },
            Reason::TimestampsChanged { old, new } => {
                format!(
                    "the `timestamps` option changed from {} to {}",
                    starlark_bool(*old),
                    starlark_bool(*new)
                )
            },
            Reason::MachineNamesChanged { old, new } => {
                format!(
                    "the `machine` names changed from {} to {}",
                    patterns(old),
                    patterns(new)
                )
            },
            Reason::InputsChanged { inputs } => self.inputs_short(inputs),
            Reason::MachineChanged { changes } => {
                format!(
                    "the machine state changed: {}",
                    machine_changes_text(changes)
                )
            },
            Reason::Always => "marked `always`".to_owned(),
            Reason::TaskCacheNever => "its task is marked `cache=never`".to_owned(),
            Reason::NotCached { why, inputs } => {
                let why = self.not_cached(why);
                if inputs.is_empty() {
                    format!("the previous run could not be cached: {why}")
                } else {
                    format!(
                        "the previous run could not be cached: {why}; {}",
                        self.inputs_short(inputs)
                    )
                }
            },
        }
    }

    /// One reason in full, on one or more lines.
    #[must_use]
    pub fn reason_full(
        &self,
        reason: &Reason,
    ) -> Vec<String> {
        match reason {
            Reason::ArgumentsChanged { old, new } => {
                let mut lines = vec![
                    "the arguments changed:".to_owned(),
                    format!("  old: {}", old.join(" ")),
                    format!("  new: {}", new.join(" ")),
                ];
                lines.extend(
                    Self::words(old, new)
                        .into_iter()
                        .map(|word| format!("  {word}")),
                );
                lines
            },
            Reason::EnvironmentChanged { changes } => {
                let mut lines = vec!["the environment changed:".to_owned()];
                lines.extend(
                    changes
                        .iter()
                        .map(|change| format!("  {}", self.env_change(change))),
                );
                lines
            },
            Reason::InputsChanged { inputs } => {
                let mut lines = vec![format!("{} changed:", count(inputs.len(), "input"))];
                lines.extend(self.inputs_full(inputs));
                lines
            },
            Reason::MachineChanged { changes } => {
                let mut lines = vec!["the machine state changed:".to_owned()];
                lines.extend(changes.iter().map(|change| format!("  {change}")));
                lines
            },
            Reason::NotCached { why, inputs } => {
                let mut lines = vec![format!(
                    "the previous run could not be cached: {}",
                    self.not_cached(why)
                )];
                if !inputs.is_empty() {
                    lines.push(format!("{} changed since:", count(inputs.len(), "input")));
                    lines.extend(self.inputs_full(inputs));
                }
                lines
            },
            Reason::NeverRun
            | Reason::ProgramChanged { .. }
            | Reason::WorkingDirectoryChanged { .. }
            | Reason::StdinChanged { .. }
            | Reason::CaptureChanged { .. }
            | Reason::IgnoreChanged { .. }
            | Reason::TrackChanged { .. }
            | Reason::TimestampsChanged { .. }
            | Reason::MachineNamesChanged { .. }
            | Reason::Always
            | Reason::TaskCacheNever => vec![self.reason_line(reason)],
        }
    }

    /// The warning line of an explanation whose self-inflicted inputs also made the
    /// step run in its task's previous build.
    fn repeated_line(
        &self,
        paths: &[PathBuf],
    ) -> String {
        let shown: Vec<String> = paths.iter().map(|path| self.quoted_path(path)).collect();
        format!(
            "warning: re-ran in consecutive builds for the same self-inflicted reason: {} \
             changed, and the build itself changes it every time; add an ignore pattern if \
             it is not a real input",
            shown.join(", ")
        )
    }

    /// The one-line reason of a verdict, as `kizz build` prints it for each command.
    #[must_use]
    pub fn one_line(
        &self,
        verdict: &Verdict,
    ) -> String {
        match verdict {
            Verdict::Cached => CACHED.to_owned(),
            Verdict::Ran(explanation) => {
                let reasons: Vec<String> = explanation
                    .reasons
                    .iter()
                    .map(|reason| self.reason_line(reason))
                    .collect();
                reasons.join("; ")
            },
        }
    }

    /// The full reasons of a verdict, as `--explain` and `kizz why` print them:
    /// `ran because:` and every reason in full, or the cached line, then the
    /// warning for consecutive builds when it applies.
    #[must_use]
    pub fn full(
        &self,
        verdict: &Verdict,
    ) -> Vec<String> {
        match verdict {
            Verdict::Cached => vec![CACHED.to_owned()],
            Verdict::Ran(explanation) => self.explanation(explanation),
        }
    }

    /// The full reasons of an explanation.
    fn explanation(
        &self,
        explanation: &Explanation,
    ) -> Vec<String> {
        let mut lines = vec!["ran because:".to_owned()];
        for reason in &explanation.reasons {
            lines.extend(
                self.reason_full(reason)
                    .into_iter()
                    .map(|line| format!("  {line}")),
            );
        }
        if !explanation.repeated.is_empty() {
            lines.push(self.repeated_line(&explanation.repeated));
        }
        lines
    }

    /// The loud warning for a step that re-ran in consecutive builds for the same
    /// self-inflicted reason, as the build summary and `kizz why` show it.
    #[must_use]
    pub fn repeated(
        &self,
        warning: &RepeatedWarning,
    ) -> String {
        let step = match &warning.task {
            Some(task) => self.step_of(task, warning.position),
            None => format!("step {}", warning.position.saturating_add(1)),
        };
        let shown: Vec<String> = warning
            .paths
            .iter()
            .map(|path| self.quoted_path(path))
            .collect();
        format!(
            "{step} ({}) re-ran in consecutive builds for the same self-inflicted reason: {} \
             changed, and the build itself changes it every time; add an ignore pattern if \
             it is not a real input",
            quoted(&warning.command),
            shown.join(", ")
        )
    }
}

/// Unit tests for the rendering of the parts of explanations.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]
mod tests {
    use kizz_store::TaskId;

    use super::*;

    /// A renderer with base `/w` and home `/home/u`.
    fn render(verbose: bool) -> Render {
        Render {
            base: PathBuf::from("/w"),
            home: Some(PathBuf::from("/home/u")),
            verbose,
        }
    }

    #[test]
    fn paths_are_relative_to_the_base_or_the_home_directory() {
        let render = render(false);
        assert_eq!(render.path(Path::new("/w")), ".");
        assert_eq!(render.path(Path::new("/w/a/b")), "a/b");
        assert_eq!(render.path(Path::new("/home/u/.cache/x")), "~/.cache/x");
        assert_eq!(render.path(Path::new("/etc/hosts")), "/etc/hosts");
    }

    #[test]
    fn tasks_and_access_kinds_read_as_output_shows_them() {
        let render = render(false);
        let mut label = TaskLabel {
            task: TaskId {
                file: PathBuf::from("/w/a/kizz.md"),
                index: 0,
            },
            line: 3,
            heading: Some("Build".to_owned()),
        };
        assert_eq!(render.task(&label), "a/kizz.md:3 (Build)");
        label.heading = None;
        assert_eq!(render.task(&label), "a/kizz.md:3");
        label.line = 0;
        assert_eq!(render.task(&label), "a/kizz.md");
        assert_eq!(
            access_kinds(AccessBits(AccessBits::READ | AccessBits::LIST)),
            "read, list"
        );
        assert_eq!(
            access_kinds(AccessBits(AccessBits::EXEC | AccessBits::XATTR)),
            "exec, xattr"
        );
    }

    #[test]
    fn words_compare_the_arguments_one_by_one() {
        let old = ["cc", "-O2", "a.c"].map(str::to_owned);
        let new = ["cc", "-O0", "a.c", "-g"].map(str::to_owned);
        assert_eq!(Render::words(&old, &new), [
            "word 2: `-O2` became `-O0`",
            "word 4 added: `-g`"
        ]);
        assert_eq!(Render::words(&new, &old), [
            "word 2: `-O0` became `-O2`",
            "word 4 removed: `-g`"
        ]);
    }
}
