// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Task blocks in `kizz.md` files.
//!
//! A task is a fenced code block, with a backtick or tilde fence, whose info string is
//! `starlark` followed by one or more target names. The nearest heading above the
//! block names the task. A `starlark` block nested in another fenced block is that
//! block's content, not a task, and a `starlark` block without targets is not a task
//! either, but gets a warning.

use std::path::{Path, PathBuf};

use kizz_store::TaskId;
use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};

use crate::{project::ConfigError, targets::is_valid_target, warning::Warning};

/// The first word of the info string of every task block.
pub const TASK_LANGUAGE: &str = "starlark";

/// A task: one `starlark` block with targets in a `kizz.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// The task's identity: the absolute path of its `kizz.md` and its position among
    /// the task blocks of that file.
    pub id: TaskId,
    /// The path of the task's `kizz.md`, relative to the directory discovery started
    /// in.
    pub relative_file: PathBuf,
    /// The line of the block's opening fence, counting from 1.
    pub line: usize,
    /// The nearest heading above the block, which names the task, if there is one.
    pub heading: Option<String>,
    /// The block's distinct targets, in the order written.
    pub targets: Vec<String>,
    /// The block's Starlark source.
    pub source: String,
}

impl Task {
    /// The task as output shows it: `path/kizz.md:LINE (Heading)`.
    #[must_use]
    pub fn location(&self) -> String {
        crate::warning::location(&self.relative_file, self.line, self.heading.as_deref())
    }
}

/// What parsing one `kizz.md` found.
#[derive(Debug, Default)]
pub struct ParsedFile {
    /// The tasks, in file order.
    pub tasks: Vec<Task>,
    /// The warnings, in file order.
    pub warnings: Vec<Warning>,
    /// The configuration errors, in file order.
    pub errors: Vec<ConfigError>,
}

/// A `starlark` block whose end has not been reached yet.
struct OpenBlock {
    /// The line of the opening fence, counting from 1.
    line: usize,
    /// The words of the info string after `starlark`.
    targets: Vec<String>,
    /// The block's content so far.
    source: String,
}

/// Parse the task blocks of one `kizz.md`.
///
/// `file` is the absolute path of the `kizz.md`, which identifies its tasks, and
/// `relative_file` is its path relative to the directory discovery started in, which
/// messages show.
#[must_use]
pub fn parse_tasks(
    file: &Path,
    relative_file: &Path,
    source: &str,
) -> ParsedFile {
    let line_starts = line_starts(source);
    let mut parsed = ParsedFile::default();
    let mut heading: Option<String> = None;
    let mut open_heading: Option<String> = None;
    let mut open_block: Option<OpenBlock> = None;

    for (event, range) in Parser::new(source).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { .. }) => open_heading = Some(String::new()),
            Event::End(TagEnd::Heading(_)) => {
                if let Some(text) = open_heading.take() {
                    let text = text.trim();
                    heading = (!text.is_empty()).then(|| text.to_owned());
                }
            },
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) => {
                let mut words = info.split_whitespace();
                if words.next() == Some(TASK_LANGUAGE) {
                    open_block = Some(OpenBlock {
                        line: line_of(&line_starts, range.start),
                        targets: words.map(str::to_owned).collect(),
                        source: String::new(),
                    });
                }
            },
            Event::Text(text) | Event::Code(text) => {
                if let Some(block) = open_block.as_mut() {
                    block.source.push_str(&text);
                } else if let Some(open) = open_heading.as_mut() {
                    open.push_str(&text);
                }
            },
            Event::SoftBreak | Event::HardBreak => {
                if let Some(open) = open_heading.as_mut() {
                    open.push(' ');
                }
            },
            Event::End(TagEnd::CodeBlock) => {
                if let Some(block) = open_block.take() {
                    finish_block(&mut parsed, file, relative_file, heading.as_deref(), block);
                }
            },
            _ => {},
        }
    }
    parsed
}

/// Turn a finished `starlark` block into a task, a warning, or configuration errors.
fn finish_block(
    parsed: &mut ParsedFile,
    file: &Path,
    relative_file: &Path,
    heading: Option<&str>,
    block: OpenBlock,
) {
    if block.targets.is_empty() {
        parsed.warnings.push(Warning::UntaggedBlock {
            file: relative_file.to_path_buf(),
            line: block.line,
            heading: heading.map(str::to_owned),
        });
        return;
    }

    let mut valid = true;
    let mut targets: Vec<String> = Vec::new();
    for target in block.targets {
        if is_valid_target(&target) {
            if !targets.contains(&target) {
                targets.push(target);
            }
        } else {
            valid = false;
            parsed.errors.push(ConfigError::InvalidTarget {
                file: relative_file.to_path_buf(),
                line: block.line,
                heading: heading.map(str::to_owned),
                target,
            });
        }
    }
    if !valid {
        return;
    }

    let Ok(index) = u32::try_from(parsed.tasks.len()) else {
        parsed.errors.push(ConfigError::TooManyTasks {
            file: relative_file.to_path_buf(),
        });
        return;
    };
    parsed.tasks.push(Task {
        id: TaskId {
            file: file.to_path_buf(),
            index,
        },
        relative_file: relative_file.to_path_buf(),
        line: block.line,
        heading: heading.map(str::to_owned),
        targets,
        source: block.source,
    });
}

/// The byte offsets at which the lines of `source` start.
fn line_starts(source: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(
            source
                .bytes()
                .enumerate()
                .filter(|&(_, byte)| byte == b'\n')
                .map(|(offset, _)| offset.saturating_add(1)),
        )
        .collect()
}

/// The line, counting from 1, that holds the byte at `offset`.
fn line_of(
    line_starts: &[usize],
    offset: usize,
) -> usize {
    line_starts.partition_point(|&start| start <= offset)
}

/// Unit tests for task-block parsing.
#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{ParsedFile, parse_tasks};
    use crate::{project::ConfigError, warning::Warning};

    /// Parse `source` as `/work/sub/kizz.md`.
    fn parse(source: &str) -> ParsedFile {
        parse_tasks(
            Path::new("/work/sub/kizz.md"),
            Path::new("sub/kizz.md"),
            source,
        )
    }

    /// The (line, heading, targets) of each task.
    fn summary(parsed: &ParsedFile) -> Vec<(usize, Option<&str>, Vec<&str>)> {
        parsed
            .tasks
            .iter()
            .map(|task| {
                (
                    task.line,
                    task.heading.as_deref(),
                    task.targets.iter().map(String::as_str).collect(),
                )
            })
            .collect()
    }

    /// Backtick and tilde fences are both task blocks, named by the nearest heading
    /// above them, with their fence line, targets, source, and position.
    #[test]
    fn backtick_and_tilde_fences_are_tasks() {
        let parsed = parse(
            "# Project\n\nIntro.\n\n## Build the library\n\n```starlark ci check\nrun(\"make\")\n```\n\n\
             ~~~starlark  lint\nrun(\"lint\")\nrun(\"more\")\n~~~\n\n### `fmt` *check*\n\n````starlark fmt\nx = 1\n````\n",
        );
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        assert_eq!(summary(&parsed), [
            (7, Some("Build the library"), vec!["ci", "check"]),
            (11, Some("Build the library"), vec!["lint"]),
            (18, Some("fmt check"), vec!["fmt"]),
        ]);
        let sources: Vec<&str> = parsed.tasks.iter().map(|t| t.source.as_str()).collect();
        assert_eq!(sources, [
            "run(\"make\")\n",
            "run(\"lint\")\nrun(\"more\")\n",
            "x = 1\n"
        ]);
        for (index, task) in (0_u32..).zip(&parsed.tasks) {
            assert_eq!(task.id.file, Path::new("/work/sub/kizz.md"));
            assert_eq!(task.id.index, index);
            assert_eq!(task.relative_file, Path::new("sub/kizz.md"));
        }
        assert_eq!(
            parsed.tasks[0].location(),
            "sub/kizz.md:7 (Build the library)"
        );
    }

    /// A task with no heading above it has no name, and its location is the file and
    /// line alone.
    #[test]
    fn a_task_without_a_heading_has_no_name() {
        let parsed = parse("```starlark ci\nx = 1\n```\n\n# Later\n");
        assert_eq!(summary(&parsed), [(1, None, vec!["ci"])]);
        assert_eq!(parsed.tasks[0].location(), "sub/kizz.md:1");
    }

    /// A `starlark` block nested in another fenced block is content, not a task, in
    /// either fence style.
    #[test]
    fn nested_fences_are_not_tasks() {
        let parsed = parse(
            "# Docs\n\n````markdown\n```starlark ci\nx = 1\n```\n````\n\n\
             ~~~text\n```starlark ci\n```\n~~~\n\n```text\n~~~starlark ci\n~~~\n```\n\n\
             ```starlark real\ny = 2\n```\n",
        );
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        assert_eq!(summary(&parsed), [(19, Some("Docs"), vec!["real"])]);
        assert_eq!(parsed.tasks[0].id.index, 0);
    }

    /// Blocks in other languages, indented code, and info strings that only start
    /// with `starlark` are not task blocks, and get no warning.
    #[test]
    fn other_blocks_are_not_tasks() {
        let parsed = parse(
            "# Other\n\n```python ci\nx = 1\n```\n\n```\nplain\n```\n\n    starlark ci\n\n\
             ```starlark_ci ci\n```\n\n```Starlark ci\n```\n",
        );
        assert!(parsed.tasks.is_empty(), "{:?}", parsed.tasks);
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
    }

    /// A `starlark` block without targets is not a task and gets a warning that
    /// names its file, line, and heading; it takes no task position.
    #[test]
    fn untagged_blocks_warn_and_are_not_tasks() {
        let parsed = parse(
            "# Notes\n\n```starlark\nx = 1\n```\n\n~~~ starlark \n~~~\n\n```starlark ci\n```\n",
        );
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        assert_eq!(parsed.warnings, [
            Warning::UntaggedBlock {
                file: "sub/kizz.md".into(),
                line: 3,
                heading: Some("Notes".to_owned()),
            },
            Warning::UntaggedBlock {
                file: "sub/kizz.md".into(),
                line: 7,
                heading: Some("Notes".to_owned()),
            },
        ]);
        assert_eq!(
            parsed.warnings[0].to_string(),
            "sub/kizz.md:3 (Notes): this starlark block has no targets, so it is not a task"
        );
        assert_eq!(summary(&parsed), [(10, Some("Notes"), vec!["ci"])]);
        assert_eq!(parsed.tasks[0].id.index, 0);
    }

    /// Invalid target names are configuration errors that name the target, file,
    /// line, and heading, and their block is not a task.
    #[test]
    fn invalid_target_names_are_configuration_errors() {
        let parsed = parse(
            "# Release\n\n```starlark ci ship!\n```\n\n## Next\n\n```starlark a/b ok c:d\n```\n\n\
             ```starlark good\n```\n",
        );
        let errors: Vec<String> = parsed.errors.iter().map(ToString::to_string).collect();
        assert_eq!(errors, [
            "sub/kizz.md:3 (Release): invalid target name `ship!`: target names use only \
             letters, digits, `_`, `.` and `-`",
            "sub/kizz.md:8 (Next): invalid target name `a/b`: target names use only letters, \
             digits, `_`, `.` and `-`",
            "sub/kizz.md:8 (Next): invalid target name `c:d`: target names use only letters, \
             digits, `_`, `.` and `-`",
        ]);
        assert!(matches!(
            &parsed.errors[0],
            ConfigError::InvalidTarget { target, line: 3, .. } if target == "ship!"
        ));
        assert_eq!(summary(&parsed), [(11, Some("Next"), vec!["good"])]);
    }

    /// Repeated targets on one block are kept once, in the order written.
    #[test]
    fn repeated_targets_are_kept_once() {
        let parsed = parse("```starlark ci check ci\n```\n");
        assert_eq!(summary(&parsed), [(1, None, vec!["ci", "check"])]);
    }

    /// Fences inside lists and block quotes are task blocks, with the line of their
    /// fence, and setext headings name tasks.
    #[test]
    fn fences_in_containers_and_setext_headings() {
        let parsed = parse(
            "Setext title\n============\n\n* item\n\n  ```starlark ci\n  x = 1\n  ```\n\n\
             > ```starlark check\n> y = 2\n> ```\n",
        );
        assert_eq!(summary(&parsed), [
            (6, Some("Setext title"), vec!["ci"]),
            (10, Some("Setext title"), vec!["check"]),
        ]);
    }
}
