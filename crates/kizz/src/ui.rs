// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Terminal output helpers for the `kizz` command.
//!
//! Every line that kizz prints goes through [`out`] or [`err`], which write to the
//! locked standard output or standard error with `Write::write_fmt` and flush it.
//! The `print!` family of macros panics when a write fails, as it does on a closed
//! pipe such as `kizz list | head -1`, because Rust ignores `SIGPIPE`; a write or a
//! flush that fails here is dropped instead, and kizz goes on: it fails no step,
//! task or command for it, and its exit status is the one that its work gives
//! ("Kizz never panics"). The lines that task programs print and the help and
//! errors of the command line go the same way.

use std::{
    fmt,
    io::{self, Write},
};

use bpaf::ParseFailure;
use console::style;

build_info::build_info!(fn build_metadata);

/// FIGlet-style `kizz` banner lines printed above command output.
const BANNER_LINES: [&str; 5] = [
    r" _    _",
    r"| | _(_)________",
    r"| |/ / |_  /_  /",
    r"|   <| |/ / / /",
    r"|_|\_\_/___/___|",
];

/// ANSI 256-color ramp used to render the banner from blue to purple.
const BANNER_COLORS: [u8; 5] = [33, 39, 63, 99, 129];

/// Write `text` to `stream` and flush it. A write or a flush that fails, such as
/// one to a closed pipe, is dropped, where the `print!` macros would panic.
fn write_to(
    mut stream: impl Write,
    text: fmt::Arguments<'_>,
) {
    let written = stream.write_fmt(text).and_then(|()| stream.flush());
    // Output that cannot be written is dropped, and kizz goes on.
    drop(written);
}

/// Write `text` to standard output, dropping a failure.
fn out(text: fmt::Arguments<'_>) {
    write_to(io::stdout().lock(), text);
}

/// Write `text` to standard error, dropping a failure.
fn err(text: fmt::Arguments<'_>) {
    write_to(io::stderr().lock(), text);
}

/// Print the startup banner, with the version and commit from the build metadata.
pub(crate) fn print_banner() {
    let build_metadata = build_metadata();
    let git_hash = build_metadata
        .version_control
        .as_ref()
        .map_or("unknown", |version_control| {
            match version_control {
                build_info::VersionControl::Git(git) => git.commit_short_id.as_str(),
            }
        });

    out(format_args!("\n"));
    for (line, color) in BANNER_LINES.iter().zip(BANNER_COLORS) {
        out(format_args!("{}\n", style(line).color256(color).bold()));
    }
    out(format_args!(
        "{}\n",
        style("Traced, cached builds from kizz.md files").yellow()
    ));
    out(format_args!(
        "{}\n",
        style(format!(
            "v{} ({}) - Copyright (c) 2026, Sakura Industries LLC - AGPLv3 Only",
            build_metadata.crate_info.version, git_hash
        ))
        .dim()
    ));
    out(format_args!("\n"));
}

/// Print the message of a command line that did not parse into a command: help or
/// the version to standard output, a shell completion as it is, or the error to
/// standard error after `Error: `.
///
/// This is what `ParseFailure::print_message` prints without the `color` feature of
/// `bpaf`, which kizz does not enable, at `bpaf`'s width of 100 columns, but written
/// through [`out`] and [`err`], where `print_message` uses the `print!` macros.
pub(crate) fn print_parse_failure(failure: &ParseFailure) {
    let (to_stderr, text) = parse_failure_message(failure);
    if to_stderr {
        err(format_args!("{text}"));
    } else {
        out(format_args!("{text}"));
    }
}

/// The message that [`print_parse_failure`] prints for `failure`, and whether it
/// goes to standard error.
fn parse_failure_message(failure: &ParseFailure) -> (bool, String) {
    match failure {
        ParseFailure::Stdout(message, full) => (false, format!("{}\n", message.monochrome(*full))),
        ParseFailure::Completion(text) => (false, text.clone()),
        ParseFailure::Stderr(message) => (true, format!("Error: {}\n", message.monochrome(true))),
    }
}

/// Print the scaffold message of a command that is parsed but not implemented yet.
pub(crate) fn print_stub(
    command_name: &str,
    summary: &str,
    target: Option<String>,
) {
    let target = target.unwrap_or_else(|| "<none>".to_owned());
    out(format_args!(
        "{}\n{}\n{} {}\n",
        style(format!("command scaffolded: {command_name}"))
            .yellow()
            .bold(),
        style(summary).dim(),
        style("target:").bold(),
        target
    ));
    out(format_args!(
        "{}\n",
        style("This command is parsed but not wired to behavior yet.").dim()
    ));
}

/// Print an error message to standard error.
pub(crate) fn print_error(message: &str) {
    err(format_args!(
        "{} {}\n",
        style("error:").for_stderr().red().bold(),
        style(message).for_stderr().bold()
    ));
}

/// Print a warning to standard error, loudly: bold and colored.
pub(crate) fn print_warning(message: &str) {
    err(format_args!(
        "{}\n",
        style(format!("warning: {message}"))
            .for_stderr()
            .yellow()
            .bold()
    ));
}

/// Print to standard error that the command waits for the cache lock, and who
/// holds it.
pub(crate) fn print_waiting(holder: &str) {
    err(format_args!(
        "{} {}\n",
        style("waiting for the kizz cache, which is in use by")
            .for_stderr()
            .yellow(),
        style(holder).for_stderr().bold()
    ));
}

/// Print a question to standard error, followed by a space and no line break, so
/// the answer follows it on the same line; the stream is flushed, so the question
/// shows before the answer is read.
pub(crate) fn print_question(question: &str) {
    err(format_args!("{} ", style(question).for_stderr().bold()));
}

/// Repeat every warning in a final summary on standard error; print nothing when
/// there are none.
pub(crate) fn print_warning_summary(warnings: &[String]) {
    if warnings.is_empty() {
        return;
    }
    err(format_args!(
        "{}\n",
        style(format!("{}:", count(warnings.len(), "warning")))
            .for_stderr()
            .yellow()
            .bold()
    ));
    for warning in warnings {
        err(format_args!(
            "  {}\n",
            style(warning).for_stderr().yellow().bold()
        ));
    }
}

/// Print one task as `path/kizz.md:LINE (Heading) [targets]`, followed by its task
/// options in their info-string form, such as `name=lint cache=never`, when it has
/// any; a task without targets shows `[]`.
pub(crate) fn print_task(
    location: &str,
    targets: &[String],
    options: &[String],
) {
    let options = if options.is_empty() {
        String::new()
    } else {
        format!(" {}", style(options.join(" ")).magenta())
    };
    out(format_args!(
        "{} {}{options}\n",
        style(location).bold(),
        style(format!("[{}]", targets.join(", "))).cyan()
    ));
}

/// Print a dimmed summary line.
pub(crate) fn print_summary(message: &str) {
    out(format_args!("{}\n", style(message).dim()));
}

/// Print a command that ran: `text` names it and gives its one-line reason.
pub(crate) fn print_ran(text: &str) {
    out(format_args!("{} {text}\n", style("ran").green().bold()));
}

/// Print a command that came from the cache: `text` names it.
pub(crate) fn print_cached(text: &str) {
    out(format_args!(
        "{} {}\n",
        style("cached").dim(),
        style(text).dim()
    ));
}

/// Print one line of detail under a command, indented.
pub(crate) fn print_detail(line: &str) {
    out(format_args!("  {line}\n"));
}

/// Print one line to standard output: a line that a task program printed, already
/// prefixed with its task, or a line of `kizz cache`.
pub(crate) fn print_line(line: &str) {
    out(format_args!("{line}\n"));
}

/// Print `text` to standard output as it is, such as a report that ends with its
/// own line break.
pub(crate) fn print_text(text: &str) {
    out(format_args!("{text}"));
}

/// Print one indented line of detail under an error, to standard error.
pub(crate) fn print_error_detail(line: &str) {
    err(format_args!("  {}\n", style(line).for_stderr().bold()));
}

/// Print to standard error that `task` failed, with `message`, which holds the
/// error, the Starlark stack, and the failing command's captured output, indented.
pub(crate) fn print_task_failure(
    task: &str,
    message: &str,
) {
    err(format_args!(
        "{} {}\n",
        style("error:").for_stderr().red().bold(),
        style(format!("{task} failed:")).for_stderr().bold()
    ));
    for line in message.lines() {
        err(format_args!("  {line}\n"));
    }
}

/// Print to standard error a task that failed in the pass that ended the build,
/// with the paths it looked for and did not find.
pub(crate) fn print_failed_task(
    task: &str,
    missing: &[String],
) {
    err(format_args!(
        "{} {}\n",
        style("failed:").for_stderr().red().bold(),
        style(task).for_stderr().bold()
    ));
    if missing.is_empty() {
        err(format_args!("  it looked for no path that is missing\n"));
    } else {
        err(format_args!(
            "  it looked for these paths and did not find them:\n"
        ));
        for path in missing {
            err(format_args!("    {path}\n"));
        }
    }
}

/// `number` followed by `singular` when `number` is 1, and by `plural` otherwise.
pub(crate) fn count_as(
    number: usize,
    singular: &str,
    plural: &str,
) -> String {
    if number == 1 {
        format!("1 {singular}")
    } else {
        format!("{number} {plural}")
    }
}

/// `number` followed by `singular`, made plural with `s` unless `number` is 1.
pub(crate) fn count(
    number: usize,
    singular: &str,
) -> String {
    if number == 1 {
        format!("1 {singular}")
    } else {
        format!("{number} {singular}s")
    }
}

/// Unit tests of the writes that fail, and of the messages of the command line.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert, and read bpaf's messages with its accessors"
)]
mod tests {
    use std::io::{self, Write};

    use super::{parse_failure_message, write_to};
    use crate::cli::cli;

    /// A stream whose every write and flush fails, as a closed pipe's does.
    struct Closed {
        /// How many writes and flushes were tried.
        tries: usize,
    }

    impl Write for Closed {
        fn write(
            &mut self,
            _bytes: &[u8],
        ) -> io::Result<usize> {
            self.tries = self.tries.saturating_add(1);
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> io::Result<()> {
            self.tries = self.tries.saturating_add(1);
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
    }

    /// A write to a stream that fails, as a closed pipe does, returns without a
    /// panic, after it tried the write.
    #[test]
    fn a_failed_write_is_dropped() {
        let mut closed = Closed { tries: 0 };
        write_to(&mut closed, format_args!("lost\n"));
        assert!(closed.tries >= 1);
    }

    /// A write that succeeds reaches the stream whole, and is flushed.
    #[test]
    fn a_write_reaches_the_stream() {
        let mut written = Vec::new();
        write_to(&mut written, format_args!("one {}\n", 2));
        assert_eq!(written, b"one 2\n");
    }

    /// Help goes to standard output and an error to standard error after
    /// `Error: `, each as `bpaf` renders it and with a final line break, as
    /// `ParseFailure::print_message` printed them.
    #[test]
    fn the_messages_of_the_command_line_are_bpaf_s() {
        let help_args: &[&str] = &["--help"];
        let help = cli().run_inner(help_args).unwrap_err();
        let (to_stderr, text) = parse_failure_message(&help);
        assert!(!to_stderr);
        assert_eq!(text, format!("{}\n", help.unwrap_stdout()));
        let wrong_args: &[&str] = &["--no-such-option"];
        let wrong = cli().run_inner(wrong_args).unwrap_err();
        let (to_stderr, text) = parse_failure_message(&wrong);
        assert!(to_stderr);
        assert_eq!(text, format!("Error: {}\n", wrong.unwrap_stderr()));
    }
}
