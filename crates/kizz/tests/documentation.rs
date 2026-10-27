// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The documentation coverage test: the reference pages of the site, under
//! `docs/content/reference/`, have an entry for everything that kizz offers.
//!
//! Each list comes from the code, so a new item without an entry fails a test:
//!
//! * the Starlark globals that kizz adds to the standard ones, the members of `env`,
//!   `fs`, `task` and `json`, `run()`'s parameters and the attributes of its result, from
//!   `kizz_core::runtime::program_names`, which reads them from the runtime;
//! * the task options, from `kizz_core::tasks::TASK_OPTION_KEYS`;
//! * the commands, options and positional arguments, from the `--help` of every command
//!   of the built `kizz`, each run with an explicit environment that holds only
//!   `KIZZ_CACHE_DIR`, set to a temporary directory;
//! * the fields of the observation report's JSON, from `kizz_core::report::JSON_FIELDS`;
//! * the environment variables, from `kizz::environment::VARIABLES`, which a scan of the
//!   crates' sources checks: every variable that their production code reads through
//!   `std::env::var` or `var_os` must be in it or among the developer variables, and
//!   every other route to the environment that the scan knows is reported with its file
//!   and line, but three known collections of the whole environment and two known lookups
//!   in it; `read_variables` lists what the scan finds and reports, and what remains
//!   outside it: reads that dependencies make, lookups through a renamed binding of a
//!   collected environment, and names that a macro pastes together.
//!
//! The entries follow the headings of the reference pages: a level 2 heading for
//! each command, global and variable, with a level 3 heading for each option,
//! positional argument, member and parameter, and a level 4 heading for each result
//! attribute; the first code span of a heading names its item. Every link to the
//! Starlark specification or to the `starlark` crate's documentation needs a title,
//! and every command or option whose help says that it is not implemented yet needs
//! an entry that starts with `Planned:`. The unit tests at the end give the checks
//! pages that lack one entry of each kind, and lists that gain an item, and show that
//! each fails with a message that names the item and the page.

#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert, and the code-span parser repeats backticks"
)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    process::Command,
};

use kizz_core::{
    report::JSON_FIELDS,
    runtime::{NameKind, ProgramName, ProgramNames, program_names},
    tasks::TASK_OPTION_KEYS,
};

/// The reference page of the command line.
const COMMAND_LINE: &str = "reference/command-line.md";

/// The reference page of the environment variables.
const ENVIRONMENT: &str = "reference/environment.md";

/// The reference page of the observation report.
const EXECUTE_REPORT: &str = "reference/execute-report.md";

/// The reference page of `kizz.md`, with the task options.
const KIZZ_MD: &str = "reference/kizz-md.md";

/// The reference page of the Starlark runtime.
const STARLARK: &str = "reference/starlark.md";

/// The heading of the task options on [`KIZZ_MD`].
const TASK_OPTIONS: &str = "Task options";

/// The written name of `run()`, whose section holds its parameters and the
/// attributes of its result.
const RUN: &str = "run()";

/// The words of a help text that mark a command or an option as planned.
const NOT_IMPLEMENTED: &str = "not implemented yet";

/// The start of the entry of a planned command or option.
const PLANNED: &str = "Planned:";

/// The starts of the addresses of Starlark's language documentation: its
/// specification and the documentation of the `starlark` crates.
const STARLARK_LINKS: [&str; 2] = [
    "https://github.com/bazelbuild/starlark",
    "https://docs.rs/starlark",
];

/// The directory of the site's pages.
fn content_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/content")
}

/// The text of the page `name`, a path under `docs/content/`.
fn read_page(name: &str) -> io::Result<String> {
    fs::read_to_string(content_dir().join(name))
}

/// One heading of a page.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Heading {
    /// The number of `#` characters that open it, from 1 to 6.
    level: usize,
    /// The text after the `#` characters, trimmed.
    text: String,
    /// The contents of the code spans of the text, in order.
    spans: Vec<String>,
    /// The first line of text after the heading, trimmed, or empty when the next
    /// heading or the end of the page comes first.
    first_line: String,
}

impl Heading {
    /// The first code span of the heading, which names its item.
    fn name(&self) -> Option<&str> {
        self.spans.first().map(String::as_str)
    }
}

/// The contents of the code spans of `text`, in order: each span is a run of
/// backticks, then text, then a run of the same length, and one space at each end
/// is dropped when both are there.
fn code_spans(text: &str) -> Vec<String> {
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some((_before, after)) = rest.split_once('`') {
        let extra = after
            .chars()
            .take_while(|&character| character == '`')
            .count();
        let fence = "`".repeat(extra.saturating_add(1));
        let after = after.trim_start_matches('`');
        let Some((span, next)) = after.split_once(&fence) else {
            break;
        };
        let span = span
            .strip_prefix(' ')
            .and_then(|span| span.strip_suffix(' '))
            .unwrap_or(span);
        spans.push(span.to_owned());
        rest = next;
    }
    spans
}

/// The fence that `line` opens or closes, as its character and length, when it is a
/// fence line of backticks or tildes, indented by at most three spaces.
fn fence(line: &str) -> Option<(char, usize)> {
    let indent = line
        .chars()
        .take_while(|&character| character == ' ')
        .count();
    if indent > 3 {
        return None;
    }
    let trimmed = line.trim_start();
    let character = trimmed.chars().next().filter(|&c| c == '`' || c == '~')?;
    let length = trimmed.chars().take_while(|&c| c == character).count();
    (length >= 3).then_some((character, length))
}

/// The lines of `text` outside its front matter and its fenced code blocks, with
/// their numbers, counting from 1.
fn prose_lines(text: &str) -> Vec<(usize, &str)> {
    let mut lines = Vec::new();
    let mut front_matter = text.starts_with("+++");
    let mut open: Option<(char, usize)> = None;
    for (number, line) in (1_usize..).zip(text.lines()) {
        if front_matter {
            front_matter = number == 1 || line.trim_end() != "+++";
            continue;
        }
        if let Some((character, length)) = open {
            let closes = fence(line).is_some_and(|(other, other_length)| {
                other == character
                    && other_length >= length
                    && line.trim().chars().all(|c| c == character)
            });
            if closes {
                open = None;
            }
            continue;
        }
        if let Some(opened) = fence(line) {
            open = Some(opened);
            continue;
        }
        lines.push((number, line));
    }
    lines
}

/// The headings of the Markdown `text`, outside front matter and fenced code blocks.
fn headings(text: &str) -> Vec<Heading> {
    let mut found: Vec<Heading> = Vec::new();
    for (_number, line) in prose_lines(text) {
        let level = line
            .chars()
            .take_while(|&character| character == '#')
            .count();
        let text = line.trim_start_matches('#');
        if (1..=6).contains(&level) && text.starts_with(' ') {
            found.push(Heading {
                level,
                text: text.trim().to_owned(),
                spans: code_spans(text),
                first_line: String::new(),
            });
        } else if let Some(last) = found.last_mut()
            && last.first_line.is_empty()
        {
            line.trim().clone_into(&mut last.first_line);
        }
    }
    found
}

/// The headings below the first heading of `level` that `matches` accepts, up to the
/// next heading of that level or a higher one, or `None` when no heading matches.
fn section(
    headings: &[Heading],
    level: usize,
    matches: impl Fn(&Heading) -> bool,
) -> Option<Vec<&Heading>> {
    let mut rest = headings
        .iter()
        .skip_while(|heading| !(heading.level == level && matches(heading)));
    rest.next()?;
    Some(rest.take_while(|heading| heading.level > level).collect())
}

/// The headings below the first heading of `level` named `name`, as [`section`]
/// finds them.
fn named_section<'a>(
    headings: &'a [Heading],
    level: usize,
    name: &str,
) -> Option<Vec<&'a Heading>> {
    section(headings, level, |heading| heading.name() == Some(name))
}

/// Whether one of `headings` has `level` and is named `name`.
fn has_entry<'a>(
    headings: impl IntoIterator<Item = &'a Heading>,
    level: usize,
    name: &str,
) -> bool {
    headings
        .into_iter()
        .any(|heading| heading.level == level && heading.name() == Some(name))
}

/// The problem of the page `page`, which has no entry for `item`, with where the
/// entry belongs.
fn missing(
    page: &str,
    item: &str,
    place: &str,
) -> String {
    format!("{page}: no entry for {item}: {place}")
}

/// The problems of the Starlark reference `text`, named `page`, for the globals,
/// members, `run()` parameters and result attributes of `names`.
fn check_starlark(
    page: &str,
    text: &str,
    names: &ProgramNames,
) -> Vec<String> {
    let headings = headings(text);
    let mut problems = Vec::new();
    for global in &names.globals {
        let written = global.written();
        if !has_entry(&headings, 2, &written) {
            problems.push(missing(
                page,
                &format!("the Starlark global `{written}`"),
                &format!("a level 2 heading named `{written}`"),
            ));
        }
    }
    for member in &names.members {
        let written = member.written();
        let module = member.name.split_once('.').map_or("", |(module, _)| module);
        let found =
            named_section(&headings, 2, module).is_some_and(|below| has_entry(below, 3, &written));
        if !found {
            problems.push(missing(
                page,
                &format!("the member `{written}`"),
                &format!("a level 3 heading named `{written}` under `{module}`"),
            ));
        }
    }
    let run = named_section(&headings, 2, RUN).unwrap_or_default();
    for parameter in &names.run_parameters {
        if !has_entry(run.iter().copied(), 3, parameter) {
            problems.push(missing(
                page,
                &format!("the `{RUN}` parameter `{parameter}`"),
                &format!("a level 3 heading named `{parameter}` under `{RUN}`"),
            ));
        }
    }
    for attribute in &names.result_attributes {
        if !has_entry(run.iter().copied(), 4, attribute) {
            problems.push(missing(
                page,
                &format!("the `{RUN}` result attribute `{attribute}`"),
                &format!("a level 4 heading named `{attribute}` under `{RUN}`"),
            ));
        }
    }
    problems
}

/// The problems of the `kizz.md` reference `text`, named `page`, for the task
/// options whose keys are `keys`: each needs a level 3 heading under "Task options"
/// whose name starts with the key and `=`, such as `name=` or `cache=never`.
fn check_task_options(
    page: &str,
    text: &str,
    keys: &[&str],
) -> Vec<String> {
    let headings = headings(text);
    let options = section(&headings, 2, |heading| heading.text == TASK_OPTIONS).unwrap_or_default();
    keys.iter()
        .filter(|key| {
            let prefix = format!("{key}=");
            !options.iter().any(|heading| {
                heading.level == 3 && heading.name().is_some_and(|name| name.starts_with(&prefix))
            })
        })
        .map(|key| {
            missing(
                page,
                &format!("the task option `{key}=`"),
                &format!("a level 3 heading named `{key}=...` under \"{TASK_OPTIONS}\""),
            )
        })
        .collect()
}

/// The problems of the reference `text`, named `page`, for `items` of the kind
/// `kind`, each of which needs a heading of `level` named after it.
fn check_entries<'a>(
    page: &str,
    text: &str,
    kind: &str,
    level: usize,
    items: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let headings = headings(text);
    items
        .into_iter()
        .filter(|item| !has_entry(&headings, level, item))
        .map(|item| {
            missing(
                page,
                &format!("{kind} `{item}`"),
                &format!("a level {level} heading named `{item}`"),
            )
        })
        .collect()
}

/// One option of a command, as its help lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OptionHelp {
    /// The forms of the option, without a value, such as `-v` and `--verbose`.
    forms: Vec<String>,
    /// The option's description, its lines joined with spaces.
    description: String,
}

/// The help of one command.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct CommandHelp {
    /// The command as typed, such as `kizz` or `kizz cache reset`.
    command: String,
    /// The first line of the help, which says what the command does.
    summary: String,
    /// Every other line of the help outside its lists' entries, trimmed: a longer
    /// description, the usage line and the lists' headings.
    other_lines: Vec<String>,
    /// The options, `-h` and `--help` included.
    options: Vec<OptionHelp>,
    /// The names of the positional arguments, such as `TARGETS`.
    positionals: Vec<String>,
    /// The names of the subcommands, such as `build`.
    subcommands: Vec<String>,
}

/// The kinds of list in a help text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelpList {
    /// "Available positional items:".
    Positionals,
    /// Any other list, of options or of subcommands.
    Other,
}

/// The help of `command`, read from its help text `text`, as `bpaf` prints it: the
/// summary, then lists whose entries are indented by four or eight spaces, with
/// longer indentation for the lines that continue a description, which only an
/// option's description keeps.
fn parse_help(
    command: &str,
    text: &str,
) -> CommandHelp {
    let mut help = CommandHelp {
        command: command.to_owned(),
        ..CommandHelp::default()
    };
    let mut list = HelpList::Other;
    let mut after_option = false;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        if help.summary.is_empty() {
            line.trim().clone_into(&mut help.summary);
            continue;
        }
        if !line.starts_with(' ') {
            help.other_lines.push(line.trim().to_owned());
            list = if line == "Available positional items:" {
                HelpList::Positionals
            } else {
                HelpList::Other
            };
            after_option = false;
            continue;
        }
        let entry = line.trim_start();
        let indent = line
            .chars()
            .take_while(|&character| character == ' ')
            .count();
        if indent > 8 {
            if after_option && let Some(option) = help.options.last_mut() {
                option.description.push(' ');
                option.description.push_str(entry.trim());
            }
            continue;
        }
        let (head, description) = entry.split_once("  ").unwrap_or((entry, ""));
        after_option = head.starts_with('-');
        if after_option {
            help.options.push(OptionHelp {
                forms: head
                    .split(", ")
                    .map(|form| {
                        form.split_once('=')
                            .map_or(form, |(name, _)| name)
                            .to_owned()
                    })
                    .collect(),
                description: description.trim().to_owned(),
            });
        } else if list == HelpList::Positionals {
            help.positionals.push(head.to_owned());
        } else {
            help.subcommands.push(head.to_owned());
        }
    }
    help
}

/// The help text of `kizz` with the words `path`, then `--help`, from the built
/// binary, run with an environment that holds only a temporary `KIZZ_CACHE_DIR`.
fn help_text(path: &[String]) -> io::Result<String> {
    let cache = tempfile::tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_kizz"))
        .args(path)
        .arg("--help")
        .env_clear()
        .env("KIZZ_CACHE_DIR", cache.path())
        .current_dir(cache.path())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "kizz {path:?} --help exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The help of `kizz` and of every command under it, read from the built binary,
/// in the order of a walk from `kizz` down.
fn all_helps() -> io::Result<Vec<CommandHelp>> {
    let mut helps = Vec::new();
    let mut pending: Vec<Vec<String>> = vec![Vec::new()];
    while let Some(path) = pending.pop() {
        let command = if path.is_empty() {
            "kizz".to_owned()
        } else {
            format!("kizz {}", path.join(" "))
        };
        let help = parse_help(&command, &help_text(&path)?);
        for subcommand in help.subcommands.iter().rev() {
            let mut next = path.clone();
            next.push(subcommand.clone());
            pending.push(next);
        }
        helps.push(help);
    }
    Ok(helps)
}

/// Whether the entry under `heading` starts with [`PLANNED`].
fn marked_planned(heading: &Heading) -> bool {
    heading.first_line.starts_with(PLANNED)
}

/// The problems of the command-line reference `text`, named `page`, for the
/// commands of `helps`: each command needs a level 2 heading named after it, with a
/// level 3 heading under it whose code spans give each form of each option and the
/// name of each positional argument; a command or an option whose help says that it
/// is not implemented yet needs an entry that starts with "Planned:".
fn check_command_line(
    page: &str,
    text: &str,
    helps: &[CommandHelp],
) -> Vec<String> {
    let headings = headings(text);
    let mut problems = Vec::new();
    for help in helps {
        let command = &help.command;
        let Some(below) = named_section(&headings, 2, command) else {
            problems.push(missing(
                page,
                &format!("the command `{command}`"),
                &format!("a level 2 heading named `{command}`"),
            ));
            continue;
        };
        let planned = help.summary.contains(NOT_IMPLEMENTED)
            || help
                .other_lines
                .iter()
                .any(|line| line.contains(NOT_IMPLEMENTED));
        if planned
            && !headings.iter().any(|heading| {
                heading.level == 2 && heading.name() == Some(command) && marked_planned(heading)
            })
        {
            problems.push(format!(
                "{page}: the entry of the planned command `{command}` does not start with \"{PLANNED}\""
            ));
        }
        let entries: Vec<&Heading> = below
            .into_iter()
            .filter(|heading| heading.level == 3)
            .collect();
        let entry_of = |name: &str| {
            entries
                .iter()
                .find(|heading| heading.spans.iter().any(|span| span == name))
                .copied()
        };
        for option in &help.options {
            for form in &option.forms {
                match entry_of(form) {
                    None => {
                        problems.push(missing(
                            page,
                            &format!("the option `{form}` of `{command}`"),
                            &format!(
                                "a level 3 heading with the code span `{form}` under `{command}`"
                            ),
                        ));
                    },
                    Some(heading)
                        if option.description.contains(NOT_IMPLEMENTED)
                            && !marked_planned(heading) =>
                    {
                        problems.push(format!(
                            "{page}: the entry of the planned option `{form}` of `{command}` does not start with \
                             \"{PLANNED}\""
                        ));
                    },
                    Some(_) => {},
                }
            }
        }
        for positional in &help.positionals {
            if entry_of(positional).is_none() {
                problems.push(missing(
                    page,
                    &format!("the positional argument `{positional}` of `{command}`"),
                    &format!(
                        "a level 3 heading with the code span `{positional}` under `{command}`"
                    ),
                ));
            }
        }
    }
    problems
}

/// Whether `tail`, the text after a link's address, starts with a title: spaces,
/// then a nonempty text between double quotes, single quotes or parentheses.
fn has_title(tail: &str) -> bool {
    if !tail.starts_with([' ', '\t']) {
        return false;
    }
    let tail = tail.trim_start();
    [('"', '"'), ('\'', '\''), ('(', ')')]
        .iter()
        .any(|&(open, close)| {
            tail.strip_prefix(open)
                .and_then(|rest| rest.split_once(close))
                .is_some_and(|(title, _)| !title.trim().is_empty())
        })
}

/// The problems of the page `text`, named `page`, for its links to Starlark's
/// language documentation: each must be an inline link or a link reference
/// definition with a title, which browsers show on hover.
fn check_link_titles(
    page: &str,
    text: &str,
) -> Vec<String> {
    let mut problems = Vec::new();
    for (number, line) in prose_lines(text) {
        for start in STARLARK_LINKS {
            for (at, _) in line.match_indices(start) {
                let (Some(before), Some(after)) = (line.get(..at), line.get(at..)) else {
                    continue;
                };
                let address = after.split([' ', '\t', ')', '>']).next().unwrap_or(after);
                let tail = after.get(address.len()..).unwrap_or("");
                let definition =
                    before.trim_start().starts_with('[') && before.trim_end().ends_with("]:");
                let inline = before.ends_with("](");
                if !((definition || inline) && has_title(tail)) {
                    problems.push(format!(
                        "{page}:{number}: the link to {address} has no title"
                    ));
                }
            }
        }
    }
    problems
}

/// The Markdown pages under `dir`, by their paths under it, sorted.
fn markdown_pages(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut pages = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "md") {
                pages.push(path);
            }
        }
    }
    pages.sort();
    Ok(pages)
}

/// The lines of the Rust source `text` that production code holds, with their
/// numbers, counting from 1: every line except those of a top-level module that
/// `#[cfg(test)]` marks, from the attribute to the line where the module's braces
/// balance, so that a closing line such as `} // tests` ends it. Braces in string,
/// raw string and character literals and in comments do not count.
fn production_lines(text: &str) -> Vec<(usize, &str)> {
    let mut lines = Vec::new();
    let mut after_cfg_test = false;
    let mut in_test_module = false;
    let mut depth = 0_usize;
    let mut lexed = Lexed::Code;
    for (number, line) in (1_usize..).zip(text.lines()) {
        if in_test_module {
            in_test_module = !closes(line, &mut depth, &mut lexed);
            continue;
        }
        if line == "#[cfg(test)]" {
            after_cfg_test = true;
            continue;
        }
        if after_cfg_test {
            let attribute =
                line.starts_with("#[") || line.starts_with(' ') || line.starts_with(")]");
            if attribute {
                continue;
            }
            after_cfg_test = false;
            let module = ["mod ", "pub mod ", "pub(crate) mod "]
                .iter()
                .any(|start| line.starts_with(start));
            if module && line.ends_with('{') {
                depth = 0;
                lexed = Lexed::Code;
                in_test_module = !closes(line, &mut depth, &mut lexed);
                continue;
            }
        }
        lines.push((number, line));
    }
    lines
}

/// Where the reader of a test module's braces is, carried from one line to the
/// next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lexed {
    /// In code, where braces count.
    Code,
    /// In a string literal.
    Str,
    /// In a raw string literal that ends with `"` and this many `#`.
    Raw(usize),
    /// In a block comment, nested this many times.
    Block(usize),
}

/// Read `line` from `lexed`, adding each `{` of code to `depth` and taking each `}`
/// of code from it, and leave in `lexed` where the line ends; whether a `}` brought
/// `depth` to 0, which ends the module. A line comment ends the line's code, and
/// a `'` that starts no character literal is a lifetime.
fn closes(
    line: &str,
    depth: &mut usize,
    lexed: &mut Lexed,
) -> bool {
    let mut rest = line;
    let mut previous: Option<char> = None;
    while let Some(character) = rest.chars().next() {
        let mut advance = character.len_utf8();
        let after = rest.get(advance..).unwrap_or_default();
        match *lexed {
            Lexed::Code => {
                if rest.starts_with("//") {
                    return false;
                }
                if rest.starts_with("/*") {
                    *lexed = Lexed::Block(1);
                    advance = 2;
                } else if character == '"' {
                    *lexed = Lexed::Str;
                } else if character == 'r'
                    && !previous.is_some_and(|before| identifier_character(before) && before != 'b')
                {
                    let hashes = after
                        .len()
                        .saturating_sub(after.trim_start_matches('#').len());
                    if after
                        .get(hashes..)
                        .is_some_and(|text| text.starts_with('"'))
                    {
                        *lexed = Lexed::Raw(hashes);
                        advance = hashes.saturating_add(2);
                    }
                } else if character == '\'' {
                    advance = character_literal(after).map_or(1, |length| length.saturating_add(1));
                } else if character == '{' {
                    *depth = depth.saturating_add(1);
                } else if character == '}' {
                    *depth = depth.saturating_sub(1);
                    if *depth == 0 {
                        return true;
                    }
                }
            },
            Lexed::Str => {
                if character == '\\' {
                    advance =
                        advance.saturating_add(after.chars().next().map_or(0, char::len_utf8));
                } else if character == '"' {
                    *lexed = Lexed::Code;
                }
            },
            Lexed::Raw(hashes) => {
                let closing = after
                    .len()
                    .saturating_sub(after.trim_start_matches('#').len());
                if character == '"' && closing >= hashes {
                    *lexed = Lexed::Code;
                    advance = hashes.saturating_add(1);
                }
            },
            Lexed::Block(nesting) => {
                if rest.starts_with("*/") {
                    *lexed = if nesting > 1 {
                        Lexed::Block(nesting.saturating_sub(1))
                    } else {
                        Lexed::Code
                    };
                    advance = 2;
                } else if rest.starts_with("/*") {
                    *lexed = Lexed::Block(nesting.saturating_add(1));
                    advance = 2;
                }
            },
        }
        previous = Some(character);
        rest = rest.get(advance..).unwrap_or_default();
    }
    false
}

/// The length of the rest of a character literal, `after` being the text after its
/// opening `'`, up to and including its closing `'`; `None` when the `'` starts a
/// lifetime instead, such as `'a`.
fn character_literal(after: &str) -> Option<usize> {
    if let Some(escaped) = after.strip_prefix('\\') {
        let skipped = escaped.chars().next().map_or(0, char::len_utf8);
        let closing = escaped.get(skipped..)?.find('\'')?;
        return Some(skipped.saturating_add(closing).saturating_add(2));
    }
    let mut characters = after.chars();
    let first = characters.next()?;
    (characters.next() == Some('\'')).then(|| first.len_utf8().saturating_add(1))
}

/// One string constant of production code.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StringConstant {
    /// The constant's name, such as `PATH_VARIABLE`.
    name: String,
    /// Its value, such as `PATH`.
    value: String,
    /// Whether its documentation says that it is a variable.
    documented_as_variable: bool,
}

/// The string constants of `lines`: each line `const NAME: &str = "VALUE";`, with
/// any visibility, and whether the documentation lines just above it say
/// "variable".
fn string_constants(lines: &[(usize, &str)]) -> Vec<StringConstant> {
    let mut constants = Vec::new();
    let mut variable_doc = false;
    for &(_number, line) in lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with("///") {
            variable_doc |= trimmed.contains("variable");
            continue;
        }
        if trimmed.starts_with("#[") {
            continue;
        }
        let declaration = ["pub const ", "pub(crate) const ", "const "]
            .iter()
            .find_map(|start| trimmed.strip_prefix(start));
        if let Some((name, value)) = declaration.and_then(|rest| rest.split_once(": &str = \""))
            && let Some((value, _)) = value.split_once('"')
        {
            constants.push(StringConstant {
                name: name.to_owned(),
                value: value.to_owned(),
                documented_as_variable: variable_doc,
            });
        }
        variable_doc = false;
    }
    constants
}

/// Whether `value` looks like the name of an environment variable: capital ASCII
/// letters, digits and `_`, starting with a letter.
fn variable_like(value: &str) -> bool {
    value.starts_with(|character: char| character.is_ascii_uppercase())
        && value.chars().all(|character| {
            character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_'
        })
}

/// The functions of `std::env` that read one variable from the process
/// environment, each matched as a whole word with any path or none before it, so
/// that `var` is found as `var`, `env::var`, `std::env::var` or `e::var` alike,
/// and `vars`, `vars_os`, `var_os_count` and `variable` stay out. A read can name
/// neither function without the text of its name: an import that renames one,
/// such as `use std::env::var as get;`, holds it and is reported.
const READS: [&str; 2] = ["var_os", "var"];

/// The words that a renaming `as` follows when it can give `std::env` another
/// name: `env`, as in `use std::env as e;`, and `self`, as in
/// `use std::env::{self as e};`, whose line need not hold `env`.
const RENAMED: [&str; 2] = ["env", "self"];

/// Whether `after`, the text after a mention of a word, starts with the keyword
/// `as`, so that the word is renamed.
fn renames(after: &str) -> bool {
    after
        .trim_start()
        .strip_prefix("as")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

/// Whether `character` can be part of a Rust identifier.
fn identifier_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

/// The text after each mention of `read` in `line` as a whole word: not preceded
/// or followed by a character of an identifier.
fn mentions<'line>(
    line: &'line str,
    read: &str,
) -> Vec<&'line str> {
    words(line, read)
        .into_iter()
        .map(|(_before, after)| after)
        .collect()
}

/// The text before and the text after each mention of `word` in `line` as a whole
/// word: not preceded or followed by a character of an identifier.
fn words<'line>(
    line: &'line str,
    word: &str,
) -> Vec<(&'line str, &'line str)> {
    line.match_indices(word)
        .filter_map(|(start, _)| {
            let before = line.get(..start)?;
            let after = line.get(start.saturating_add(word.len())..)?;
            let whole =
                !before.ends_with(identifier_character) && !after.starts_with(identifier_character);
            whole.then_some((before, after))
        })
        .collect()
}

/// The words that read the process environment outside `std::env`, each found
/// as a whole word: the C library's `getenv` and `secure_getenv`, as `libc`
/// offers them, and its array `environ`, which is also the last word of the path
/// `/proc/self/environ`, so a read of that file is found by the same rule.
const RAW_READS: [&str; 3] = ["getenv", "secure_getenv", "environ"];

/// The functions of `std::env` that collect the whole environment: `vars_os`,
/// found wherever it stands as a whole word, and `vars`, found as a whole word
/// that is called, follows `::` or stands in a `use` statement, because kizz's code
/// also names local maps of variables `vars`.
const COLLECTIONS: [&str; 2] = ["vars_os", "vars"];

/// The name of the bindings and fields that hold a collected environment in
/// kizz's code: `caller`, in `kizz build` and `kizz run` and in the runtime that
/// `env.caller` and `env.inherit` read.
const COLLECTED: &str = "caller";

/// A line of production code that the scan knows and lets pass once in its file.
#[derive(Debug, Clone, Copy)]
struct KnownLine {
    /// The file, by its path under the workspace's `crates/`.
    file: &'static str,
    /// The line's text without its indentation.
    line: &'static str,
    /// Why the line does not read a variable that kizz has to document.
    reason: &'static str,
}

/// The three places that collect the whole environment with `vars_os`, each
/// allowed once in its file.
const KNOWN_COLLECTIONS: [KnownLine; 3] = [
    KnownLine {
        file: "kizz/src/build.rs",
        line: "let caller: BTreeMap<String, String> = env::vars_os()",
        reason: "the caller environment of `kizz build`, handed to task programs for \
                 `env.caller` and `env.inherit`",
    },
    KnownLine {
        file: "kizz/src/run.rs",
        line: "let caller: BTreeMap<String, String> = env::vars_os()",
        reason: "the caller environment of `kizz run`, handed to task programs for \
                 `env.caller` and `env.inherit`",
    },
    KnownLine {
        file: "kizz/src/execute.rs",
        line: "env: std::env::vars_os().collect(),",
        reason: "the environment that `kizz execute` passes to its program unchanged",
    },
];

/// The two lookups in a collected `caller` environment that the scan knows, each
/// allowed once in its file.
const KNOWN_LOOKUPS: [KnownLine; 2] = [
    KnownLine {
        file: "kizz-core/src/runtime.rs",
        line: "Ok(state.caller.get(name).cloned())",
        reason: "`env.caller`, which looks up the name that the task program gives",
    },
    KnownLine {
        file: "kizz-core/src/runtime.rs",
        line: "if let Some(value) = state.caller.get(name) {",
        reason: "`env.inherit`, which looks up the names that the task program gives",
    },
];

/// Whether `line` of the file `name` is one of `known` that `used` has not
/// counted yet, counting it if so.
fn known_once(
    known: &[KnownLine],
    used: &mut BTreeSet<usize>,
    name: &str,
    line: &str,
) -> bool {
    let found = known
        .iter()
        .enumerate()
        .find(|(index, entry)| {
            entry.file == name && entry.line == line.trim() && !used.contains(index)
        })
        .map(|(index, _entry)| index);
    found.is_some_and(|index| used.insert(index))
}

/// Whether `line` starts a `use` statement, with any visibility.
fn starts_use(line: &str) -> bool {
    let trimmed = line.trim_start();
    ["use ", "pub use ", "pub(crate) use ", "pub(super) use "]
        .iter()
        .any(|start| trimmed.starts_with(start))
}

/// The problems of the routes other than [`READS`] by which the production lines
/// `lines` of the file `name` can read the environment, each with its file and
/// line, in the order of the lines:
///
/// * every whole-word [`RAW_READS`], `/proc/self/environ` included;
/// * every call `env(` with any path or none before it, such as bpaf's environment
///   fallback `.env("X")` on a named option, `env("X")` in a `#[bpaf(...)]` attribute or
///   `bpaf::env("X")`; `env!(` is a macro and stays out;
/// * every collection of [`COLLECTIONS`] but the [`KNOWN_COLLECTIONS`];
/// * every method call or index on a binding or field named [`COLLECTED`], such as
///   `caller.get("X")`, `caller.contains_key("X")`, `state.caller.iter()` or
///   `caller["X"]`, but the [`KNOWN_LOOKUPS`];
/// * every `use` statement whose text holds the whole word `env` and a `*`, such as `use
///   std::env::*;`, after which `vars` could be named without a path, at the statement's
///   first line.
fn route_problems(
    name: &str,
    lines: &[(usize, &str)],
) -> Vec<String> {
    let mut problems = Vec::new();
    let mut collections = BTreeSet::new();
    let mut lookups = BTreeSet::new();
    let mut statement: Option<(usize, bool, bool)> = None;
    for &(number, line) in lines {
        if statement.is_none() && starts_use(line) {
            statement = Some((number, false, false));
        }
        let in_use = statement.is_some();
        for word in RAW_READS {
            if !words(line, word).is_empty() {
                problems.push(format!(
                    "{name}:{number}: `{word}` reads the environment outside `std::env`, which the \
                     scan reports wherever it occurs"
                ));
            }
        }
        if mentions(line, "env")
            .iter()
            .any(|after| after.starts_with('('))
        {
            problems.push(format!(
                "{name}:{number}: `env(` may read a variable through bpaf's environment fallback, \
                 which the scan reports wherever it occurs"
            ));
        }
        for word in COLLECTIONS {
            let collects = words(line, word).iter().any(|&(before, after)| {
                word == "vars_os"
                    || in_use
                    || before.ends_with("::")
                    || after.trim_start().starts_with('(')
            });
            if collects && !known_once(&KNOWN_COLLECTIONS, &mut collections, name, line) {
                problems.push(format!(
                    "{name}:{number}: `{word}` collects the whole environment, so the scan cannot \
                     tell which variables are looked up in it"
                ));
            }
        }
        let looks_up = mentions(line, COLLECTED).iter().any(|after| {
            after.starts_with('[')
                || after
                    .strip_prefix('.')
                    .is_some_and(|method| method.starts_with(identifier_character))
        });
        if looks_up && !known_once(&KNOWN_LOOKUPS, &mut lookups, name, line) {
            problems.push(format!(
                "{name}:{number}: a lookup in `{COLLECTED}`, a collected environment, may read a \
                 variable that the scan cannot tell"
            ));
        }
        if let Some((start, env, glob)) = statement.as_mut() {
            *env |= !words(line, "env").is_empty();
            *glob |= line.contains('*');
            if line.contains(';') {
                if *env && *glob {
                    problems.push(format!(
                        "{name}:{start}: a glob import beside `env` may name the functions of \
                         `std::env` without a path, which the scan reports wherever it occurs"
                    ));
                }
                statement = None;
            }
        }
    }
    problems
}

/// What follows a mention of a read function, `after` being the rest of its line.
#[derive(Debug, PartialEq, Eq)]
enum Mention<'line> {
    /// A call whose whole argument is on the line, with that argument.
    Call(&'line str),
    /// A call whose argument does not end on the line.
    Unclosed,
    /// No call: the function is named, such as passed to `map` or imported.
    Named,
}

/// Which [`Mention`] `after`, the text after a read function's name, is.
fn mention(after: &str) -> Mention<'_> {
    let Some(inside) = after.trim_start().strip_prefix('(') else {
        return Mention::Named;
    };
    let mut depth = 1_usize;
    for (index, character) in inside.char_indices() {
        match character {
            '(' => depth = depth.saturating_add(1),
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return inside.get(..index).map_or(Mention::Unclosed, Mention::Call);
                }
            },
            _ => {},
        }
    }
    Mention::Unclosed
}

/// The variables that the production code of `sources`, each a file's name and its
/// text, reads from the process environment: the argument of every call of
/// [`READS`], a string literal or a string constant of any of the sources, and the
/// value of every string constant that its documentation calls a variable, such as
/// the `PATH` that `kizz execute` reads from the environment that it collects.
///
/// What it finds and what it reports, each problem with its file and line:
///
/// * Every mention of a name of [`READS`] as a whole word, with any path or none before
///   it, in the production lines is either a call that the scan resolves or a problem: an
///   argument that names neither a literal nor a constant, a call whose argument does not
///   end on the call's line, and a read function that is named without a call, such as
///   `.map(env::var_os)`, the braced import `use std::env::{current_dir, var};` or the
///   renaming import `use std::env::var as get;`. So a bare `var("X")` after a braced
///   import, and `e::var("X")` after `use std::env as e;`, are found as reads of `X`.
/// * Every word of [`RENAMED`] followed by `as`, such as `use std::env as e;`, is a
///   problem, so that no other name of `std::env` passes unreported.
/// * The other routes to the environment are problems, as [`route_problems`] lists them:
///   `getenv`, `secure_getenv`, `environ` and `/proc/self/environ`; every call `env(`,
///   bpaf's environment fallback among them; every collection of the whole environment
///   with `vars` or `vars_os` but the [`KNOWN_COLLECTIONS`]; every lookup in a collected
///   environment named `caller` but the [`KNOWN_LOOKUPS`]; and every glob import beside
///   `env`.
///
/// Production lines are all but those of test modules, which end where their
/// braces balance ([`production_lines`]). Mentions in comments and strings count
/// as well, so the scan can report a line that reads nothing.
///
/// What remains outside the scan:
///
/// * the reads that dependencies make;
/// * a lookup by a fixed name in a collected environment through a binding of another
///   name than `caller`, such as `let all = caller;` and then `all.get("X")`, or in a
///   known collection's chain on the lines after it, whose variable only the
///   documentation of its constant names, as for `PATH`;
/// * a name that a macro pastes together, such as one of the `paste` crate's, or that a
///   string is built from in pieces, which holds none of the words above.
///
/// Apart from those, Rust reaches a function only through its own name or an
/// import that holds that name, so production code cannot read a variable through
/// any of these routes without a mention that the scan finds.
fn read_variables(sources: &[(String, String)]) -> Result<BTreeSet<String>, Vec<String>> {
    let production: Vec<(&str, Vec<(usize, &str)>)> = sources
        .iter()
        .map(|(name, text)| (name.as_str(), production_lines(text)))
        .collect();
    let mut values: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut variables = BTreeSet::new();
    for (_name, lines) in &production {
        for constant in string_constants(lines) {
            if constant.documented_as_variable && variable_like(&constant.value) {
                variables.insert(constant.value.clone());
            }
            values
                .entry(constant.name)
                .or_default()
                .insert(constant.value);
        }
    }
    let mut problems = Vec::new();
    for (name, lines) in &production {
        for &(number, line) in lines {
            for read in READS {
                for after in mentions(line, read) {
                    let argument = match mention(after) {
                        Mention::Call(argument) => argument.trim().trim_start_matches('&'),
                        Mention::Unclosed => {
                            problems.push(format!(
                                "{name}:{number}: the argument of `{read}(` does not end on its line, \
                                 so the scan cannot tell which variable it reads"
                            ));
                            continue;
                        },
                        Mention::Named => {
                            problems.push(format!(
                                "{name}:{number}: `{read}` is named without a call, so the scan cannot \
                                 tell which variable it reads"
                            ));
                            continue;
                        },
                    };
                    if let Some(literal) = argument
                        .strip_prefix('"')
                        .and_then(|rest| rest.split_once('"'))
                    {
                        variables.insert(literal.0.to_owned());
                        continue;
                    }
                    let constant = argument.rsplit("::").next().unwrap_or(argument);
                    match values.get(constant) {
                        Some(found) => variables.extend(found.iter().cloned()),
                        None => problems.push(format!(
                            "{name}:{number}: cannot tell which variable `{read}({argument})` reads"
                        )),
                    }
                }
            }
            for word in RENAMED {
                if mentions(line, word).into_iter().any(renames) {
                    problems.push(format!(
                        "{name}:{number}: `{word} as` may give `std::env` another name, which the scan \
                         reports wherever it occurs"
                    ));
                }
            }
        }
        problems.extend(route_problems(name, lines));
    }
    if problems.is_empty() {
        Ok(variables)
    } else {
        Err(problems)
    }
}

/// The Rust sources under `src/` of every crate of the workspace, by their paths
/// under the workspace's `crates/`, sorted.
fn crate_sources() -> io::Result<Vec<(String, String)>> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut sources = Vec::new();
    let mut pending: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(&crates)? {
        let src = entry?.path().join("src");
        if src.is_dir() {
            pending.push(src);
        }
    }
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let name = path
                    .strip_prefix(&crates)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                sources.push((name, fs::read_to_string(&path)?));
            }
        }
    }
    sources.sort();
    Ok(sources)
}

/// The problems of a scan that found `read`, for each variable that neither
/// `kizz::environment::VARIABLES` nor `kizz::environment::DEVELOPER_VARIABLES`
/// lists.
fn unlisted(read: &BTreeSet<String>) -> Vec<String> {
    read.iter()
        .filter(|variable| {
            !kizz::environment::VARIABLES.contains(&variable.as_str())
                && !kizz::environment::DEVELOPER_VARIABLES.contains(&variable.as_str())
        })
        .map(|variable| {
            format!("kizz reads the environment variable `{variable}`, which kizz::environment does not list")
        })
        .collect()
}

/// Fail with every problem in `problems`, one to a line.
fn assert_none(problems: &[String]) {
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Every Starlark global that kizz adds, every member of `env`, `fs`, `task` and
/// `json`, every `run()` parameter and every attribute of a `run()` result has its
/// entry in the Starlark reference.
#[test]
fn the_reference_covers_the_starlark_runtime() -> io::Result<()> {
    let names = program_names();
    assert!(
        !names.globals.is_empty() && !names.members.is_empty(),
        "{names:?}"
    );
    assert!(
        !names.run_parameters.is_empty() && !names.result_attributes.is_empty(),
        "{names:?}"
    );
    assert_none(&check_starlark(STARLARK, &read_page(STARLARK)?, &names));
    Ok(())
}

/// Every task option has its entry under "Task options" in the `kizz.md`
/// reference.
#[test]
fn the_reference_covers_the_task_options() -> io::Result<()> {
    assert_ne!(TASK_OPTION_KEYS, [""; 0]);
    assert_none(&check_task_options(
        KIZZ_MD,
        &read_page(KIZZ_MD)?,
        TASK_OPTION_KEYS,
    ));
    Ok(())
}

/// Every field of the observation report's JSON has its entry in the report's
/// reference.
#[test]
fn the_reference_covers_the_report_fields() -> io::Result<()> {
    let fields = JSON_FIELDS.iter().map(|field| field.path);
    assert_none(&check_entries(
        EXECUTE_REPORT,
        &read_page(EXECUTE_REPORT)?,
        "the JSON field",
        3,
        fields,
    ));
    Ok(())
}

/// Every environment variable that the `kizz` command reads has its entry in the
/// reference of the environment; the developer variables, which the reference also
/// documents, are not required.
#[test]
fn the_reference_covers_the_environment_variables() -> io::Result<()> {
    assert_none(&check_entries(
        ENVIRONMENT,
        &read_page(ENVIRONMENT)?,
        "the environment variable",
        2,
        kizz::environment::VARIABLES,
    ));
    Ok(())
}

/// Every command, option and positional argument that the help of the built
/// `kizz` shows has its entry in the command-line reference, and every command and
/// option whose help says that it is not implemented yet is marked as planned.
#[test]
fn the_reference_covers_the_command_line() -> io::Result<()> {
    let helps = all_helps()?;
    let top = helps.first().map_or(0, |help| help.subcommands.len());
    assert!(top > 0, "kizz --help lists no command: {helps:?}");
    for help in &helps {
        let has_help = help
            .options
            .iter()
            .any(|option| option.forms.iter().any(|form| form == "--help"));
        assert!(
            has_help,
            "the help of `{}` has no `--help`: {help:?}",
            help.command
        );
    }
    assert_none(&check_command_line(
        COMMAND_LINE,
        &read_page(COMMAND_LINE)?,
        &helps,
    ));
    Ok(())
}

/// Every link from a page of the site to the Starlark specification or to the
/// documentation of the `starlark` crates has a title.
#[test]
fn starlark_links_have_titles() -> io::Result<()> {
    let dir = content_dir();
    let pages = markdown_pages(&dir)?;
    assert!(!pages.is_empty(), "no page under {}", dir.display());
    let mut problems = Vec::new();
    for path in pages {
        let name = path
            .strip_prefix(&dir)
            .unwrap_or(&path)
            .display()
            .to_string();
        problems.extend(check_link_titles(&name, &fs::read_to_string(&path)?));
    }
    assert_none(&problems);
    Ok(())
}

/// The scan of the crates' sources finds every variable that kizz's own code reads,
/// and `kizz::environment` lists each one it finds.
#[test]
fn kizz_reads_only_listed_variables() -> io::Result<()> {
    let sources = crate_sources()?;
    let read = match read_variables(&sources) {
        Ok(read) => read,
        Err(problems) => {
            assert_none(&problems);
            BTreeSet::new()
        },
    };
    for own in [
        kizz_store::location::KIZZ_CACHE_DIR,
        kizz_store::location::XDG_CACHE_HOME,
        kizz_store::location::HOME,
        kizz_core::PATH_VARIABLE,
        kizz::environment::FAIL_THREAD_STARTS,
    ] {
        assert!(
            read.contains(own),
            "the scan did not find `{own}`: {read:?}"
        );
    }
    assert_none(&unlisted(&read));
    Ok(())
}

/// Each line that the scan knows, the three collections of the whole environment
/// and the two lookups in a collected one, stands exactly once among the
/// production lines of its file, so the lists name only lines that exist, each
/// with its reason.
#[test]
fn the_known_environment_lines_exist() -> io::Result<()> {
    let sources = crate_sources()?;
    for known in KNOWN_COLLECTIONS.iter().chain(&KNOWN_LOOKUPS) {
        let count = sources
            .iter()
            .filter(|(name, _text)| name.as_str() == known.file)
            .flat_map(|(_name, text)| production_lines(text))
            .filter(|&(_number, line)| line.trim() == known.line)
            .count();
        assert_eq!(
            count, 1,
            "{}: the known line `{}`, {}, must stand there once",
            known.file, known.line, known.reason
        );
    }
    Ok(())
}

/// `text` with the one line `line` replaced by `replacement`, failing when `text`
/// does not hold that line.
fn replace_line(
    text: &str,
    line: &str,
    replacement: &str,
) -> String {
    assert!(
        text.lines().any(|existing| existing == line),
        "no line {line:?}"
    );
    text.lines()
        .map(|existing| {
            if existing == line {
                replacement
            } else {
                existing
            }
        })
        .collect::<Vec<&str>>()
        .join("\n")
}

/// Fail unless one of `problems` names both `item` and `page`.
fn assert_names(
    problems: &[String],
    item: &str,
    page: &str,
) {
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains(item) && problem.contains(page)),
        "no problem names {item} on {page}: {problems:?}"
    );
}

/// A Starlark reference that lacks the entry of a global, of a member of each of
/// `env`, `fs`, `task` and `json`, of a `run()` parameter or of a result attribute
/// fails with a message that names the item and the page.
#[test]
fn a_missing_starlark_entry_fails() -> io::Result<()> {
    let text = read_page(STARLARK)?;
    let names = program_names();
    for (heading, item) in [
        ("## `print()`", "`print()`"),
        ("### `env.set()`", "`env.set()`"),
        ("### `fs.glob()`", "`fs.glob()`"),
        ("### `task.args`", "`task.args`"),
        ("### `json.decode()`", "`json.decode()`"),
        ("### `timestamps`", "parameter `timestamps`"),
        ("#### `cached`", "attribute `cached`"),
    ] {
        let lacking = replace_line(&text, heading, "Text instead of the heading.");
        assert_names(&check_starlark(STARLARK, &lacking, &names), item, STARLARK);
    }
    Ok(())
}

/// A `kizz.md` reference without the entry of a task option, a report reference
/// without the entry of a JSON field, and an environment reference without the
/// entry of a variable each fail with a message that names the item and the page.
#[test]
fn missing_option_field_and_variable_entries_fail() -> io::Result<()> {
    let lacking = replace_line(&read_page(KIZZ_MD)?, "### `cache=never`", "No heading.");
    assert_names(
        &check_task_options(KIZZ_MD, &lacking, TASK_OPTION_KEYS),
        "`cache=`",
        KIZZ_MD,
    );
    let lacking = replace_line(
        &read_page(EXECUTE_REPORT)?,
        "### `connections[].name`",
        "No heading.",
    );
    let fields = JSON_FIELDS.iter().map(|field| field.path);
    let problems = check_entries(EXECUTE_REPORT, &lacking, "the JSON field", 3, fields);
    assert_names(&problems, "`connections[].name`", EXECUTE_REPORT);
    let lacking = replace_line(&read_page(ENVIRONMENT)?, "## `TERM`", "No heading.");
    let problems = check_entries(
        ENVIRONMENT,
        &lacking,
        "the environment variable",
        2,
        kizz::environment::VARIABLES,
    );
    assert_names(&problems, "`TERM`", ENVIRONMENT);
    Ok(())
}

/// A command-line reference without the entry of a command, of an option or of a
/// positional argument, or with a planned command whose entry lacks its mark, fails
/// with a message that names the item and the page.
#[test]
fn missing_command_line_entries_fail() -> io::Result<()> {
    let text = read_page(COMMAND_LINE)?;
    let helps = all_helps()?;
    assert_none(&check_command_line(COMMAND_LINE, &text, &helps));
    for (heading, item) in [
        ("## `kizz cache clean`", "command `kizz cache clean`"),
        ("### `--no-cache`", "option `--no-cache` of `kizz run`"),
        ("### `-v`, `--verbose`", "option `-v` of `kizz`"),
        ("### `ARGUMENTS`", "argument `ARGUMENTS` of `kizz run`"),
    ] {
        let lacking = replace_line(&text, heading, "Text instead of the heading.");
        assert_names(
            &check_command_line(COMMAND_LINE, &lacking, &helps),
            item,
            COMMAND_LINE,
        );
    }
    let unmarked = text.replacen("Planned: `kizz graph`", "Later: `kizz graph`", 1);
    assert_ne!(unmarked, text);
    assert_names(
        &check_command_line(COMMAND_LINE, &unmarked, &helps),
        "planned command `kizz graph`",
        COMMAND_LINE,
    );
    let unmarked = text.replacen("Planned: `--parallel`", "Later: `--parallel`", 1);
    assert_ne!(unmarked, text);
    assert_names(
        &check_command_line(COMMAND_LINE, &unmarked, &helps),
        "planned option `--parallel` of `kizz build`",
        COMMAND_LINE,
    );
    Ok(())
}

/// A link to the Starlark specification or to the `starlark` crate's documentation
/// without a title fails with a message that names the link and the page, as an
/// inline link, as a link reference definition and as an autolink; links with titles
/// and links elsewhere pass, and so does an address in a fenced code block.
#[test]
fn a_starlark_link_without_a_title_fails() -> io::Result<()> {
    let text = read_page(STARLARK)?;
    assert_none(&check_link_titles(STARLARK, &text));
    let untitled = text.replacen(" \"The Starlark language specification\"", "", 1);
    assert_ne!(untitled, text);
    assert_names(
        &check_link_titles(STARLARK, &untitled),
        "https://github.com/bazelbuild/starlark/blob/master/spec.md",
        STARLARK,
    );
    let spec = "https://github.com/bazelbuild/starlark/blob/master/spec.md";
    let crate_docs = "https://docs.rs/starlark/0.14.2/starlark/";
    let page = format!(
        "+++\ntitle = \"Links\"\n+++\n\nSee [the spec]({spec} \"The specification\") and [docs]({crate_docs}).\n\
         \n<{spec}>\n\n[bare]: {crate_docs}\n[titled]: {crate_docs} \"The crate\"\n[other]: https://example.com\n\
         \n```text\n{spec}\n```\n"
    );
    let problems = check_link_titles("page.md", &page);
    assert_eq!(problems, [
        format!("page.md:5: the link to {crate_docs} has no title"),
        format!("page.md:7: the link to {spec} has no title"),
        format!("page.md:9: the link to {crate_docs} has no title"),
    ]);
    Ok(())
}

/// Each kind of item that the code gains without an entry fails the checks with a
/// message that names it and its page: a global, a member, a `run()` parameter, a
/// result attribute, a task option, a JSON field, a variable, a command, an option
/// and a positional argument.
#[test]
fn a_new_item_without_an_entry_fails() -> io::Result<()> {
    let mut names = program_names();
    names.globals.push(ProgramName {
        name: "build_info".to_owned(),
        kind: NameKind::Function,
    });
    names.members.push(ProgramName {
        name: "fs.remove".to_owned(),
        kind: NameKind::Function,
    });
    names.run_parameters.push("retries".to_owned());
    names.result_attributes.push("duration".to_owned());
    let problems = check_starlark(STARLARK, &read_page(STARLARK)?, &names);
    for item in [
        "`build_info()`",
        "`fs.remove()`",
        "parameter `retries`",
        "attribute `duration`",
    ] {
        assert_names(&problems, item, STARLARK);
    }
    let problems = check_task_options(KIZZ_MD, &read_page(KIZZ_MD)?, &["name", "cache", "retry"]);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_names(&problems, "`retry=`", KIZZ_MD);
    let problems = check_entries(
        EXECUTE_REPORT,
        &read_page(EXECUTE_REPORT)?,
        "the JSON field",
        3,
        ["commands[].duration"],
    );
    assert_names(&problems, "`commands[].duration`", EXECUTE_REPORT);
    let problems = check_entries(
        ENVIRONMENT,
        &read_page(ENVIRONMENT)?,
        "the environment variable",
        2,
        ["KIZZ_JOBS"],
    );
    assert_names(&problems, "`KIZZ_JOBS`", ENVIRONMENT);
    let mut helps = all_helps()?;
    if let Some(run) = helps.iter_mut().find(|help| help.command == "kizz run") {
        run.options.push(OptionHelp {
            forms: vec!["--dry-run".to_owned()],
            description: "Show the commands without running them.".to_owned(),
        });
        run.positionals.push("TIMEOUT".to_owned());
    }
    helps.push(CommandHelp {
        command: "kizz shell".to_owned(),
        summary: "Open a shell in a task's environment.".to_owned(),
        ..CommandHelp::default()
    });
    let problems = check_command_line(COMMAND_LINE, &read_page(COMMAND_LINE)?, &helps);
    for item in [
        "option `--dry-run` of `kizz run`",
        "argument `TIMEOUT` of `kizz run`",
        "command `kizz shell`",
    ] {
        assert_names(&problems, item, COMMAND_LINE);
    }
    Ok(())
}

/// The help parser reads `bpaf`'s layout: the summary, the other lines outside the
/// lists' entries, subcommands, options with their forms and continued
/// descriptions, and positional arguments, and it does not read a usage line that
/// wraps as an entry.
#[test]
fn help_texts_parse() {
    let text = "Run the tasks.\n\nUsage: kizz build [--explain] [--sequential |\n--parallel] [TARGETS]\n\n\
                How `kizz build` schedules tasks.\n        --sequential  Run one task at a time.\n        \
                --parallel    Run tasks in parallel (not implemented yet).\n\nAvailable positional items:\n    \
                TARGETS       One target, or several, such as\n                  `ci+check`.\n\nAvailable \
                options:\n        --color=<auto|always|never>  Color policy.\n    -v, --verbose  Print more \
                detail,\n                   such as cached commands.\n\nThe commands.\n    clean       Remove \
                entries.\n";
    let help = parse_help("kizz build", text);
    assert_eq!(help, CommandHelp {
        command: "kizz build".to_owned(),
        summary: "Run the tasks.".to_owned(),
        other_lines: [
            "Usage: kizz build [--explain] [--sequential |",
            "--parallel] [TARGETS]",
            "How `kizz build` schedules tasks.",
            "Available positional items:",
            "Available options:",
            "The commands.",
        ]
        .iter()
        .map(|&line| line.to_owned())
        .collect(),
        options: vec![
            OptionHelp {
                forms: vec!["--sequential".to_owned()],
                description: "Run one task at a time.".to_owned(),
            },
            OptionHelp {
                forms: vec!["--parallel".to_owned()],
                description: "Run tasks in parallel (not implemented yet).".to_owned(),
            },
            OptionHelp {
                forms: vec!["--color".to_owned()],
                description: "Color policy.".to_owned(),
            },
            OptionHelp {
                forms: vec!["-v".to_owned(), "--verbose".to_owned()],
                description: "Print more detail, such as cached commands.".to_owned(),
            },
        ],
        positionals: vec!["TARGETS".to_owned()],
        subcommands: vec!["clean".to_owned()],
    });
}

/// A command whose help says that it is not implemented yet on any line, not only
/// on its first, needs an entry that starts with "Planned:", and a message names
/// the command and the page when it lacks one.
#[test]
fn a_planned_command_is_found_on_any_line_of_its_help() {
    let help = parse_help(
        "kizz sync",
        "Copy the cache to another machine.\n\nCopying is not implemented yet.\n\nUsage: kizz sync\n\n\
         Available options:\n    -h, --help  Prints help information\n",
    );
    let helps = [help];
    let unmarked = "## `kizz sync`\n\nCopies the cache.\n\n### `-h`, `--help`\n\nPrints help.\n";
    assert_names(
        &check_command_line("page.md", unmarked, &helps),
        "planned command `kizz sync`",
        "page.md",
    );
    let marked = unmarked.replacen("Copies", "Planned: copies", 1);
    assert_none(&check_command_line("page.md", &marked, &helps));
}

/// Headings are read outside front matter and fenced code blocks, with their code
/// spans and the first line of their entries.
#[test]
fn headings_parse() {
    let text = "+++\ntitle = \"Page\"\n# not a heading\n+++\n\n## `kizz` and its options\n\nThe options.\n\n\
                ````markdown\n# Inside\n```starlark\n```\n````\n\n### `-v`, `--verbose`\n### ``a`b`` c\n";
    let found = headings(text);
    let summary: Vec<(usize, Vec<String>, &str)> = found
        .iter()
        .map(|heading| {
            (
                heading.level,
                heading.spans.clone(),
                heading.first_line.as_str(),
            )
        })
        .collect();
    assert_eq!(summary, [
        (2, vec!["kizz".to_owned()], "The options."),
        (3, vec!["-v".to_owned(), "--verbose".to_owned()], ""),
        (3, vec!["a`b".to_owned()], ""),
    ]);
}

/// The scan finds a variable read with a string literal, one read through a
/// constant, also by its path, and one whose constant's documentation calls it a
/// variable; it skips test modules, and it fails on an argument that names no
/// constant. A variable that the lists lack is a problem that names it. It finds
/// `var` as it finds `var_os`, with any path or none before it: a bare `var`, as
/// after a braced import, `e::var` and `my_env::var` are reads. Every read that it
/// cannot resolve is a problem with its file and line: an argument on the next
/// line, a read function passed by name or imported, braced, renamed or
/// re-exported, and an argument that is itself a call; so is every `env as` and
/// `self as`, which can rename `std::env`, also on a line of a braced import that
/// does not hold `env`. Longer names that hold a read function's name, such as
/// `var_os_count` and `variable`, are not reads, while `vars_os` and `vars` are
/// collections of the whole environment, each a problem with its file and line.
#[test]
fn the_variable_scan_finds_reads() {
    let source = "/// The variable of the jobs.\nconst JOBS: &str = \"KIZZ_JOBS\";\n\
                  /// The width.\npub const WIDTH: &str = \"COLUMNS\";\n\
                  fn f() {\n    let _ = std::env::var_os(\"KIZZ_LOG\");\n    \
                  let _ = env::var(crate::WIDTH);\n}\n\n\
                  /// Tests.\n#[cfg(test)]\n#[allow(\n    clippy::x,\n)]\nmod tests {\n    \
                  const HELPER: &str = \"KIZZ_HELPER\";\n    fn g() { std::env::var_os(HELPER); }\n}\n";
    let sources = [("a.rs".to_owned(), source.to_owned())];
    let read = read_variables(&sources).unwrap_or_default();
    let expected: BTreeSet<String> = ["COLUMNS", "KIZZ_JOBS", "KIZZ_LOG"]
        .iter()
        .map(|&name| name.to_owned())
        .collect();
    assert_eq!(read, expected);
    assert_names(&unlisted(&read), "`KIZZ_LOG`", "kizz::environment");
    let unknown = [(
        "b.rs".to_owned(),
        "fn f() { std::env::var_os(name); }\n".to_owned(),
    )];
    assert_eq!(
        read_variables(&unknown),
        Err(vec![
            "b.rs:1: cannot tell which variable `var_os(name)` reads".to_owned()
        ])
    );
    let found = [(
        "d.rs".to_owned(),
        "fn f() {\n    let _ = (var(\"KIZZ_BARE\"), e::var(\"KIZZ_RENAMED\"), my_env::var(\"KIZZ_OTHER\"));\n    \
         let _ = std::env::var(\"KIZZ_FULL\");\n}\n"
            .to_owned(),
    )];
    let expected: BTreeSet<String> = ["KIZZ_BARE", "KIZZ_FULL", "KIZZ_OTHER", "KIZZ_RENAMED"]
        .iter()
        .map(|&name| name.to_owned())
        .collect();
    assert_eq!(read_variables(&found), Ok(expected));
    let routes = [
        (
            "braced.rs".to_owned(),
            "use std::env::{current_dir, var};\nfn f() {\n    let _ = var(\"KIZZ_JOBS\");\n}\n"
                .to_owned(),
        ),
        (
            "renamed.rs".to_owned(),
            "use std::env as e;\nfn f() {\n    let _ = e::var(\"KIZZ_JOBS\");\n}\n".to_owned(),
        ),
    ];
    assert_eq!(
        read_variables(&routes),
        Err(vec![
            "braced.rs:1: `var` is named without a call, so the scan cannot tell which variable it reads"
                .to_owned(),
            "renamed.rs:1: `env as` may give `std::env` another name, which the scan reports wherever it \
             occurs"
                .to_owned(),
        ])
    );
    let unresolved = [(
        "c.rs".to_owned(),
        "use std::env::var;\nuse std::{\n    env::{self as e, var as get},\n    process::ExitCode,\n};\n\
         pub use std::env::var_os as read;\nfn f() {\n    \
         let _ = std::env::var_os(\n        \"KIZZ_SPLIT\",\n    );\n    \
         let _ = names.iter().map(env::var_os);\n    \
         let _ = (env::vars_os(), std::env::vars(), var_os_count(1), variable(2));\n    \
         let _ = env::var(name());\n    let _ = names.iter().map(var);\n    \
         let _ = e::var(\n        \"KIZZ_SPLIT\",\n    );\n}\n"
            .to_owned(),
    )];
    let named = |line: usize, read: &str| {
        format!(
            "c.rs:{line}: `{read}` is named without a call, so the scan cannot tell which variable it reads"
        )
    };
    let unclosed = |line: usize, read: &str| {
        format!(
            "c.rs:{line}: the argument of `{read}(` does not end on its line, so the scan cannot tell which \
             variable it reads"
        )
    };
    assert_eq!(
        read_variables(&unresolved),
        Err(vec![
            named(1, "var"),
            named(3, "var"),
            "c.rs:3: `self as` may give `std::env` another name, which the scan reports wherever it occurs"
                .to_owned(),
            named(6, "var_os"),
            unclosed(8, "var_os"),
            named(11, "var_os"),
            "c.rs:13: cannot tell which variable `var(name())` reads".to_owned(),
            named(14, "var"),
            unclosed(15, "var"),
            collects("c.rs", 12, "vars_os"),
            collects("c.rs", 12, "vars"),
        ])
    );
}

/// The problem of a collection of the whole environment with `word` at `line` of
/// `file`.
fn collects(
    file: &str,
    line: usize,
    word: &str,
) -> String {
    format!(
        "{file}:{line}: `{word}` collects the whole environment, so the scan cannot tell which \
         variables are looked up in it"
    )
}

/// The problem of a lookup in a collected `caller` environment at `line` of `file`.
fn looks_up(
    file: &str,
    line: usize,
) -> String {
    format!(
        "{file}:{line}: a lookup in `caller`, a collected environment, may read a variable that \
         the scan cannot tell"
    )
}

/// The source of the lines `lines`, each ended by a line break.
fn source_of(lines: &[&str]) -> String {
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// The scan reports every other route to the environment with its file and line:
/// a glob import beside `env`; `vars` in a `use` statement, called or after `::`,
/// and `vars_os` wherever it stands; bpaf's environment fallback, as `.env(` on a
/// named option, `env(` in a `#[bpaf(...)]` attribute and `bpaf::env(`; `getenv`,
/// `secure_getenv`, `environ` and a read of `/proc/self/environ`; and a method call
/// or an index on `caller`. A local map named `vars`, the macro `env!`, a `caller`
/// passed whole and the word "caller" in a string are not reported.
#[test]
fn the_variable_scan_reports_other_routes() {
    let routes = [(
        "routes.rs".to_owned(),
        source_of(&[
            "use std::env::*;",
            "use std::{",
            "    env::{self, vars},",
            "    io,",
            "};",
            "#[derive(Bpaf)]",
            "struct Cli {",
            r#"    #[bpaf(long, env("KIZZ_JOBS"))]"#,
            "    jobs: usize,",
            "}",
            "fn f() {",
            r#"    let _ = unsafe { libc::getenv(c"KIZZ_JOBS".as_ptr()) };"#,
            r#"    let _ = unsafe { libc::secure_getenv(c"KIZZ_JOBS".as_ptr()) };"#,
            "    let _ = unsafe { environ };",
            r#"    let _ = fs::read("/proc/self/environ");"#,
            r#"    let _ = long("jobs").env("KIZZ_JOBS");"#,
            r#"    let _ = bpaf::env("KIZZ_JOBS");"#,
            r#"    let _ = env!("CARGO_PKG_NAME");"#,
            r#"    let _ = env::vars_os().find(|(name, _)| name == "KIZZ_JOBS");"#,
            "    let _ = vars();",
            "    let vars = BTreeMap::new();",
            r#"    let _ = vars.get("X");"#,
            r#"    let _ = caller.get("KIZZ_JOBS");"#,
            r#"    let _ = caller.contains_key("KIZZ_JOBS");"#,
            r#"    let _ = caller["KIZZ_JOBS"];"#,
            r#"    let _ = (run(caller), "the caller.");"#,
            "}",
        ]),
    )];
    let raw = |line: usize, word: &str| {
        format!(
            "routes.rs:{line}: `{word}` reads the environment outside `std::env`, which the scan \
             reports wherever it occurs"
        )
    };
    let fallback = |line: usize| {
        format!(
            "routes.rs:{line}: `env(` may read a variable through bpaf's environment fallback, \
             which the scan reports wherever it occurs"
        )
    };
    assert_eq!(
        read_variables(&routes),
        Err(vec![
            "routes.rs:1: a glob import beside `env` may name the functions of `std::env` without a \
             path, which the scan reports wherever it occurs"
                .to_owned(),
            collects("routes.rs", 3, "vars"),
            fallback(8),
            raw(12, "getenv"),
            raw(13, "secure_getenv"),
            raw(14, "environ"),
            raw(15, "environ"),
            fallback(16),
            fallback(17),
            collects("routes.rs", 19, "vars_os"),
            collects("routes.rs", 20, "vars"),
            looks_up("routes.rs", 23),
            looks_up("routes.rs", 24),
            looks_up("routes.rs", 25),
        ])
    );
}

/// Each known collection of the whole environment and each known lookup in a
/// collected one passes once in its own file: the same line again, or in another
/// file, is a problem with its file and line, as is any other lookup.
#[test]
fn the_variable_scan_lets_known_lines_pass_once() {
    let collection = "    let caller: BTreeMap<String, String> = env::vars_os()";
    let sources = [
        (
            "kizz/src/build.rs".to_owned(),
            source_of(&[
                "fn f() {",
                collection,
                "        .filter_map(owned)",
                "        .collect();",
                collection,
                "        .collect();",
                "}",
            ]),
        ),
        (
            "kizz/src/other.rs".to_owned(),
            source_of(&["fn f() {", collection, "        .collect();", "}"]),
        ),
        (
            "kizz-core/src/runtime.rs".to_owned(),
            source_of(&[
                "fn f() {",
                "    Ok(state.caller.get(name).cloned())",
                "    if let Some(value) = state.caller.get(name) {",
                "    }",
                r#"    let _ = state.caller.get("KIZZ_JOBS");"#,
                "}",
            ]),
        ),
    ];
    assert_eq!(
        read_variables(&sources),
        Err(vec![
            collects("kizz/src/build.rs", 5, "vars_os"),
            collects("kizz/src/other.rs", 2, "vars_os"),
            looks_up("kizz-core/src/runtime.rs", 5),
        ])
    );
}

/// A test module ends where its braces balance, so a closing line such as
/// `} // tests` ends it and the production code below it is scanned, while braces
/// in character, string, raw string and multi-line string literals, in nested
/// block comments and in line comments do not count, and lifetimes are no
/// character literals.
#[test]
fn the_variable_scan_ends_test_modules_where_their_braces_balance() {
    let text = source_of(&[
        "fn f() {}",
        "#[cfg(test)]",
        "mod tests {",
        "    const OPEN: char = '{';",
        r"    const ESCAPED: char = '\'';",
        r#"    const TEXT: &str = "} \" }";"#,
        r##"    const RAW: &str = r#"} " }"#;"##,
        r#"    const LONG: &str = "{"#,
        r#"} still in the string";"#,
        "    /* } /* nested } */ } */",
        "    fn g<'a>(name: &'a str) { std::env::var_os(name); } // }",
        "} // tests",
        "fn h() {",
        r#"    let _ = std::env::var_os("KIZZ_AFTER");"#,
        "}",
    ]);
    let sources = [("modules.rs".to_owned(), text.clone())];
    let expected: BTreeSet<String> = ["KIZZ_AFTER".to_owned()].into();
    assert_eq!(read_variables(&sources), Ok(expected));
    let lines: Vec<usize> = production_lines(&text)
        .into_iter()
        .map(|(number, _line)| number)
        .collect();
    assert_eq!(lines, [1, 13, 14, 15]);
}
