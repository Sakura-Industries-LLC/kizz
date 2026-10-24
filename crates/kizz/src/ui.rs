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
