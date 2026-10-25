// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Terminal output helpers for the `kizz` command.

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

    println!();
    for (line, color) in BANNER_LINES.iter().zip(BANNER_COLORS) {
        println!("{}", style(line).color256(color).bold());
    }
    println!(
        "{}",
        style("Traced, cached builds from kizz.md files").yellow()
    );
    println!(
        "{}",
        style(format!(
            "v{} ({}) - Copyright (c) 2026, Sakura Industries LLC - AGPLv3 Only",
            build_metadata.crate_info.version, git_hash
        ))
        .dim()
    );
    println!();
}

/// Print the scaffold message of a command that is parsed but not implemented yet.
pub(crate) fn print_stub(
    command_name: &str,
    summary: &str,
    target: Option<String>,
) {
    let target = target.unwrap_or_else(|| "<none>".to_owned());
    println!(
        "{}\n{}\n{} {}",
        style(format!("command scaffolded: {command_name}"))
            .yellow()
            .bold(),
        style(summary).dim(),
        style("target:").bold(),
        target
    );
    println!(
        "{}",
        style("This command is parsed but not wired to behavior yet.").dim()
    );
}

/// Print an error message to standard error.
pub(crate) fn print_error(message: &str) {
    eprintln!(
        "{} {}",
        style("error:").for_stderr().red().bold(),
        style(message).for_stderr().bold()
    );
}

/// Print a warning to standard error, loudly: bold and colored.
pub(crate) fn print_warning(message: &str) {
    eprintln!(
        "{}",
        style(format!("warning: {message}"))
            .for_stderr()
            .yellow()
            .bold()
    );
}

/// Print to standard error that the command waits for the cache lock, and who
/// holds it.
pub(crate) fn print_waiting(holder: &str) {
    eprintln!(
        "{} {}",
        style("waiting for the kizz cache, which is in use by")
            .for_stderr()
            .yellow(),
        style(holder).for_stderr().bold()
    );
}

/// Print a question to standard error, followed by a space and no line break, so
/// the answer follows it on the same line.
pub(crate) fn print_question(question: &str) {
    eprint!("{} ", style(question).for_stderr().bold());
}

/// Repeat every warning in a final summary on standard error; print nothing when
/// there are none.
pub(crate) fn print_warning_summary(warnings: &[String]) {
    if warnings.is_empty() {
        return;
    }
    eprintln!(
        "{}",
        style(format!("{}:", count(warnings.len(), "warning")))
            .for_stderr()
            .yellow()
            .bold()
    );
    for warning in warnings {
        eprintln!("  {}", style(warning).for_stderr().yellow().bold());
    }
}

/// Print one task as `path/kizz.md:LINE (Heading) [targets]`.
pub(crate) fn print_task(
    location: &str,
    targets: &[String],
) {
    println!(
        "{} {}",
        style(location).bold(),
        style(format!("[{}]", targets.join(", "))).cyan()
    );
}

/// Print a dimmed summary line.
pub(crate) fn print_summary(message: &str) {
    println!("{}", style(message).dim());
}

/// Print a command that ran: `text` names it and gives its one-line reason.
pub(crate) fn print_ran(text: &str) {
    println!("{} {text}", style("ran").green().bold());
}

/// Print a command that came from the cache: `text` names it.
pub(crate) fn print_cached(text: &str) {
    println!("{} {}", style("cached").dim(), style(text).dim());
}

/// Print one line of detail under a command, indented.
pub(crate) fn print_detail(line: &str) {
    println!("  {line}");
}

/// Print one line that a task program printed, already prefixed with its task.
pub(crate) fn print_line(line: &str) {
    println!("{line}");
}

/// Print one indented line of detail under an error, to standard error.
pub(crate) fn print_error_detail(line: &str) {
    eprintln!("  {}", style(line).for_stderr().bold());
}

/// Print to standard error that `task` failed, with `message`, which holds the
/// error, the Starlark stack, and the failing command's captured output, indented.
pub(crate) fn print_task_failure(
    task: &str,
    message: &str,
) {
    eprintln!(
        "{} {}",
        style("error:").for_stderr().red().bold(),
        style(format!("{task} failed:")).for_stderr().bold()
    );
    for line in message.lines() {
        eprintln!("  {line}");
    }
}

/// Print to standard error a task that failed in the pass that ended the build,
/// with the paths it looked for and did not find.
pub(crate) fn print_failed_task(
    task: &str,
    missing: &[String],
) {
    eprintln!(
        "{} {}",
        style("failed:").for_stderr().red().bold(),
        style(task).for_stderr().bold()
    );
    if missing.is_empty() {
        eprintln!("  it looked for no path that is missing");
    } else {
        eprintln!("  it looked for these paths and did not find them:");
        for path in missing {
            eprintln!("    {path}");
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
