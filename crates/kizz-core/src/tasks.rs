// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Task blocks in `kizz.md` files.
//!
//! A task is a fenced code block, with a backtick or tilde fence, whose info string is
//! `starlark` followed by targets, task options, or both. After `starlark`, a word with
//! `=` is a task option and every other word is a target. The options are
//! `name=<name>`, which names the task with the characters of a target name, uniquely
//! within its `kizz.md`, and `cache=never`, which makes every command of the task run
//! as if it were marked `always`. The nearest heading above the block is the task's
//! heading. A `starlark` block nested in another fenced block is that block's content,
//! not a task, and a `starlark` block with neither targets nor a name is not a task
//! either, but gets a warning; its options are still checked.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use kizz_store::TaskId;
use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};

use crate::{project::ConfigError, targets::is_valid_target, warning::Warning};

/// The first word of the info string of every task block.
pub const TASK_LANGUAGE: &str = "starlark";

/// The keys of the task options, in the order output shows them.
pub const TASK_OPTION_KEYS: &[&str] = &[NAME_KEY, CACHE_KEY];

/// The key of the task option that names a task.
const NAME_KEY: &str = "name";

/// The key of the task option that chooses how a task's commands are cached.
const CACHE_KEY: &str = "cache";

/// The one value that the `cache` option accepts.
const CACHE_NEVER: &str = "never";

/// The rule every task name follows, as error messages state it.
pub const TASK_NAME_RULE: &str = "task names use only letters, digits, `_`, `.` and `-`";

/// The options of a task block: the words of its info string that hold `=`.
///
/// Task options are not part of the task program, so they change no step key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskOptions {
    /// The name that `name=<name>` gives the task, if it has one.
    pub name: Option<String>,
    /// Whether `cache=never` marks the task, so every command of the task runs as if
    /// it were marked `always`.
    pub cache_never: bool,
}

impl TaskOptions {
    /// The options in their info-string form, in the order of
    /// [`TASK_OPTION_KEYS`], such as `name=lint` and `cache=never`.
    #[must_use]
    pub fn words(&self) -> Vec<String> {
        let mut words = Vec::new();
        if let Some(name) = &self.name {
            words.push(format!("{NAME_KEY}={name}"));
        }
        if self.cache_never {
            words.push(format!("{CACHE_KEY}={CACHE_NEVER}"));
        }
        words
    }
}

/// What is wrong with one task option, a configuration error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionProblem {
    /// The word starts with `=`, so the option has no key.
    NoKey,
    /// The key is not the key of a task option.
    UnknownKey,
    /// An earlier word of the same info string already gave this key.
    RepeatedKey {
        /// The repeated key.
        key: String,
    },
    /// The option has nothing after its `=`.
    EmptyValue,
    /// The `name` option's value is not a valid task name.
    InvalidName,
    /// The `cache` option's value is not `never`.
    InvalidCache,
    /// An earlier task block of the same `kizz.md` already has this name.
    DuplicateName {
        /// The line of that block's opening fence, counting from 1.
        first_line: usize,
    },
}

impl OptionProblem {
    /// The message of this problem with the option `option`, the word as written.
    #[must_use]
    pub fn message(
        &self,
        option: &str,
    ) -> String {
        match self {
            Self::NoKey => {
                format!("task option `{option}` has no key: a task option is a `key=value` word")
            },
            Self::UnknownKey => {
                format!(
                    "unknown task option `{option}`: the task options are `name=<name>` and \
                     `cache=never`"
                )
            },
            Self::RepeatedKey { key } => {
                format!(
                    "task option `{option}` gives the key `{key}` again, which a block gives once"
                )
            },
            Self::EmptyValue => format!("task option `{option}` has an empty value"),
            Self::InvalidName => format!("invalid task name in `{option}`: {TASK_NAME_RULE}"),
            Self::InvalidCache => {
                format!("invalid value in task option `{option}`: `cache` accepts only `never`")
            },
            Self::DuplicateName { first_line } => {
                format!(
                    "task option `{option}` gives a name that the task at line {first_line} of \
                     this file already has, and a name is unique within its kizz.md"
                )
            },
        }
    }
}

/// A task: one `starlark` block with targets, a name, or both, in a `kizz.md`.
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
    /// The nearest heading above the block, the task's heading, if there is one.
    pub heading: Option<String>,
    /// The block's distinct targets, in the order written; empty for a task that
    /// only its name selects.
    pub targets: Vec<String>,
    /// The block's task options.
    pub options: TaskOptions,
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
    words: Vec<String>,
    /// The block's content so far.
    source: String,
}

/// Where a block is, as its configuration errors and warnings name it.
struct BlockPlace<'a> {
    /// The path of the `kizz.md`, relative to the directory discovery started in.
    relative_file: &'a Path,
    /// The line of the block's opening fence, counting from 1.
    line: usize,
    /// The nearest heading above the block, if there is one.
    heading: Option<&'a str>,
}

impl BlockPlace<'_> {
    /// The configuration error of the task option `option` at this place.
    fn option_error(
        &self,
        option: &str,
        problem: OptionProblem,
    ) -> ConfigError {
        ConfigError::TaskOption {
            file: self.relative_file.to_path_buf(),
            line: self.line,
            heading: self.heading.map(str::to_owned),
            option: option.to_owned(),
            problem,
        }
    }
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
    let mut names: Vec<(String, usize)> = Vec::new();
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
                        words: words.map(str::to_owned).collect(),
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
                    let place = BlockPlace {
                        relative_file,
                        line: block.line,
                        heading: heading.as_deref(),
                    };
                    finish_block(&mut parsed, &mut names, file, &place, block);
                }
            },
            _ => {},
        }
    }
    parsed
}

/// Read the task option `key=value` into `options`, given the keys that earlier
/// words of the same info string gave, `seen`, which gains `key`.
///
/// # Errors
///
/// The [`OptionProblem`] of the option, when it has one; `options` is unchanged.
fn read_option(
    key: &str,
    value: &str,
    options: &mut TaskOptions,
    seen: &mut Vec<String>,
) -> Result<(), OptionProblem> {
    if key.is_empty() {
        return Err(OptionProblem::NoKey);
    }
    if !TASK_OPTION_KEYS.contains(&key) {
        return Err(OptionProblem::UnknownKey);
    }
    if seen.iter().any(|known| known == key) {
        return Err(OptionProblem::RepeatedKey {
            key: key.to_owned(),
        });
    }
    seen.push(key.to_owned());
    if value.is_empty() {
        return Err(OptionProblem::EmptyValue);
    }
    if key == NAME_KEY {
        if !is_valid_target(value) {
            return Err(OptionProblem::InvalidName);
        }
        options.name = Some(value.to_owned());
    } else if value == CACHE_NEVER {
        options.cache_never = true;
    } else {
        return Err(OptionProblem::InvalidCache);
    }
    Ok(())
}

/// Turn a finished `starlark` block into a task, a warning, or configuration errors.
///
/// `names` holds the name and fence line of each earlier block of the file that gave
/// a valid name, and gains this block's.
fn finish_block(
    parsed: &mut ParsedFile,
    names: &mut Vec<(String, usize)>,
    file: &Path,
    place: &BlockPlace<'_>,
    block: OpenBlock,
) {
    let mut valid = true;
    let mut targets: Vec<String> = Vec::new();
    let mut target_words = 0_usize;
    let mut options = TaskOptions::default();
    let mut seen: Vec<String> = Vec::new();
    let mut name_word: Option<String> = None;
    for word in block.words {
        if let Some((key, value)) = word.split_once('=') {
            match read_option(key, value, &mut options, &mut seen) {
                Ok(()) if key == NAME_KEY => name_word = Some(word.clone()),
                Ok(()) => {},
                Err(problem) => {
                    valid = false;
                    parsed.errors.push(place.option_error(&word, problem));
                },
            }
            continue;
        }
        target_words = target_words.saturating_add(1);
        if is_valid_target(&word) {
            if !targets.contains(&word) {
                targets.push(word);
            }
        } else {
            valid = false;
            parsed.errors.push(ConfigError::InvalidTarget {
                file: place.relative_file.to_path_buf(),
                line: place.line,
                heading: place.heading.map(str::to_owned),
                target: word,
            });
        }
    }

    if target_words == 0 && !seen.iter().any(|key| key == NAME_KEY) {
        parsed.warnings.push(Warning::UntaggedBlock {
            file: place.relative_file.to_path_buf(),
            line: place.line,
            heading: place.heading.map(str::to_owned),
        });
        return;
    }

    if let (Some(name), Some(word)) = (&options.name, &name_word) {
        if let Some((_, first_line)) = names.iter().find(|(known, _)| known == name) {
            valid = false;
            parsed
                .errors
                .push(place.option_error(word, OptionProblem::DuplicateName {
                    first_line: *first_line,
                }));
        } else {
            names.push((name.clone(), place.line));
        }
    }
    if !valid {
        return;
    }

    let Ok(index) = u32::try_from(parsed.tasks.len()) else {
        parsed.errors.push(ConfigError::TooManyTasks {
            file: place.relative_file.to_path_buf(),
        });
        return;
    };
    parsed.tasks.push(Task {
        id: TaskId {
            file: file.to_path_buf(),
            index,
        },
        relative_file: place.relative_file.to_path_buf(),
        line: place.line,
        heading: place.heading.map(str::to_owned),
        targets,
        options,
        source: block.source,
    });
}

impl fmt::Display for TaskOptions {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        f.write_str(&self.words().join(" "))
    }
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
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]
mod tests {
    use std::path::Path;

    use super::{OptionProblem, ParsedFile, TaskOptions, parse_tasks};
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

    /// The messages of the configuration errors.
    fn messages(parsed: &ParsedFile) -> Vec<String> {
        parsed.errors.iter().map(ToString::to_string).collect()
    }

    /// Backtick and tilde fences are both task blocks, each with the nearest heading
    /// above it as its heading, and with its fence line, targets, source, and position.
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
            assert_eq!(task.options, TaskOptions::default());
        }
        assert_eq!(
            parsed.tasks[0].location(),
            "sub/kizz.md:7 (Build the library)"
        );
    }

    /// A task with no heading above it has the heading `None`, and its location is the
    /// file and line alone.
    #[test]
    fn a_task_without_a_heading_has_no_heading() {
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

    /// A `starlark` block with neither targets nor a name is not a task and gets a
    /// warning that names its file, line, and heading; it takes no task position.
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
            "sub/kizz.md:3 (Notes): this starlark block has neither targets nor a name, so it \
             is not a task"
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
    /// fence, and a setext heading is the heading of the tasks below it.
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

    /// Targets and options mix in any order: a word with `=` is an option, and every
    /// other word is a target.
    #[test]
    fn targets_and_options_in_any_order() {
        let parsed = parse(
            "# Lint\n\n```starlark ci name=lint check cache=never\n```\n\n\
             ```starlark cache=never name=fmt fmt\n```\n\n```starlark name=docs docs\n```\n",
        );
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        assert_eq!(summary(&parsed), [
            (3, Some("Lint"), vec!["ci", "check"]),
            (6, Some("Lint"), vec!["fmt"]),
            (9, Some("Lint"), vec!["docs"]),
        ]);
        let options: Vec<&TaskOptions> = parsed.tasks.iter().map(|task| &task.options).collect();
        assert_eq!(options, [
            &TaskOptions {
                name: Some("lint".to_owned()),
                cache_never: true,
            },
            &TaskOptions {
                name: Some("fmt".to_owned()),
                cache_never: true,
            },
            &TaskOptions {
                name: Some("docs".to_owned()),
                cache_never: false,
            },
        ]);
        let words: Vec<String> = parsed
            .tasks
            .iter()
            .map(|task| task.options.to_string())
            .collect();
        assert_eq!(words, [
            "name=lint cache=never",
            "name=fmt cache=never",
            "name=docs"
        ]);
    }

    /// `name=` takes every valid target name, and `cache=never` alone marks a task
    /// with targets.
    #[test]
    fn valid_names_and_cache_never() {
        let parsed = parse(
            "```starlark name=a.b-c_D9\n```\n\n```starlark name=0 ci\n```\n\n\
             ```starlark ci cache=never\n```\n",
        );
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        let names: Vec<Option<&str>> = parsed
            .tasks
            .iter()
            .map(|task| task.options.name.as_deref())
            .collect();
        assert_eq!(names, [Some("a.b-c_D9"), Some("0"), None]);
        let never: Vec<bool> = parsed
            .tasks
            .iter()
            .map(|task| task.options.cache_never)
            .collect();
        assert_eq!(never, [false, false, true]);
        assert_eq!(parsed.tasks[2].options.words(), ["cache=never"]);
    }

    /// An unknown key, a key given twice, an empty or invalid value, an invalid name,
    /// and a word that starts with `=` are each a configuration error that names the
    /// file, line, heading, and option, and the block is not a task.
    #[test]
    fn option_mistakes_are_configuration_errors() {
        let parsed = parse(
            "# Opts\n\n```starlark ci color=red\n```\n\n```starlark ci name=a name=b\n```\n\n\
             ```starlark ci cache=never cache=never\n```\n\n```starlark ci name=\n```\n\n\
             ```starlark ci cache=\n```\n\n```starlark ci cache=always\n```\n\n\
             ```starlark ci name=a/b\n```\n\n```starlark ci =x\n```\n\n```starlark ci name=a=b\n```\n\n\
             ```starlark good\n```\n",
        );
        assert_eq!(messages(&parsed), [
            "sub/kizz.md:3 (Opts): unknown task option `color=red`: the task options are \
             `name=<name>` and `cache=never`",
            "sub/kizz.md:6 (Opts): task option `name=b` gives the key `name` again, which a \
             block gives once",
            "sub/kizz.md:9 (Opts): task option `cache=never` gives the key `cache` again, which \
             a block gives once",
            "sub/kizz.md:12 (Opts): task option `name=` has an empty value",
            "sub/kizz.md:15 (Opts): task option `cache=` has an empty value",
            "sub/kizz.md:18 (Opts): invalid value in task option `cache=always`: `cache` accepts \
             only `never`",
            "sub/kizz.md:21 (Opts): invalid task name in `name=a/b`: task names use only \
             letters, digits, `_`, `.` and `-`",
            "sub/kizz.md:24 (Opts): task option `=x` has no key: a task option is a `key=value` \
             word",
            "sub/kizz.md:27 (Opts): invalid task name in `name=a=b`: task names use only \
             letters, digits, `_`, `.` and `-`",
        ]);
        assert!(matches!(
            &parsed.errors[1],
            ConfigError::TaskOption { line: 6, option, problem: OptionProblem::RepeatedKey { key }, .. }
                if option == "name=b" && key == "name"
        ));
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        assert_eq!(summary(&parsed), [(30, Some("Opts"), vec!["good"])]);
        assert_eq!(parsed.tasks[0].id.index, 0);
    }

    /// A name given to two tasks of one file is a configuration error on the later
    /// one, which names the earlier one's line; the same name in another file is
    /// another file's business.
    #[test]
    fn a_name_given_to_two_tasks_of_one_file_is_an_error() {
        let parsed = parse(
            "# One\n\n```starlark ci name=lint\n```\n\n## Two\n\n```starlark check name=lint\n```\n\n\
             ```starlark name=other\n```\n",
        );
        assert_eq!(messages(&parsed), [
            "sub/kizz.md:8 (Two): task option `name=lint` gives a name that the task at line 3 of \
             this file already has, and a name is unique within its kizz.md",
        ]);
        assert!(matches!(&parsed.errors[0], ConfigError::TaskOption {
            problem: OptionProblem::DuplicateName { first_line: 3 },
            ..
        }));
        assert_eq!(summary(&parsed), [
            (3, Some("One"), vec!["ci"]),
            (11, Some("Two"), Vec::new()),
        ]);
        let other = parse_tasks(
            Path::new("/work/other/kizz.md"),
            Path::new("other/kizz.md"),
            "```starlark name=lint\n```\n",
        );
        assert!(other.errors.is_empty(), "{:?}", other.errors);
    }

    /// A block with only a name is a task without targets, and draws no warning.
    #[test]
    fn a_block_with_only_a_name_is_a_task_without_targets() {
        let parsed = parse("# Deploy\n\n```starlark name=deploy\nrun(\"ship\")\n```\n");
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        assert_eq!(summary(&parsed), [(3, Some("Deploy"), Vec::new())]);
        assert_eq!(parsed.tasks[0].options.name.as_deref(), Some("deploy"));
        assert_eq!(parsed.tasks[0].source, "run(\"ship\")\n");
    }

    /// A block with only `cache=never` warns as untagged and is not a task, and the
    /// options of a block without targets or a name are still checked.
    #[test]
    fn a_block_with_only_cache_never_warns_and_is_checked() {
        let parsed =
            parse("# Odd\n\n```starlark cache=never\n```\n\n```starlark cache=sometimes\n```\n");
        assert!(parsed.tasks.is_empty(), "{:?}", parsed.tasks);
        assert_eq!(parsed.warnings, [
            Warning::UntaggedBlock {
                file: "sub/kizz.md".into(),
                line: 3,
                heading: Some("Odd".to_owned()),
            },
            Warning::UntaggedBlock {
                file: "sub/kizz.md".into(),
                line: 6,
                heading: Some("Odd".to_owned()),
            },
        ]);
        assert_eq!(messages(&parsed), [
            "sub/kizz.md:6 (Odd): invalid value in task option `cache=sometimes`: `cache` \
             accepts only `never`",
        ]);
    }
}
