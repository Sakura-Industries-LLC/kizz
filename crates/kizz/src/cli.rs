// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Command-line interface for the `kizz` tool.
//!
//! This module owns parsing and dispatch only. Every command is parsed with all of
//! its options; `kizz build` runs in the `build` module, `kizz list` in the `list`
//! module, and `kizz why` in the `why` module, and the other commands are scaffolds
//! until later phases implement them.

use std::{fmt, path::PathBuf, process::ExitCode, str::FromStr};

use bpaf::{Args, Bpaf, ParseFailure};

use crate::ui;

/// The exit status for usage and configuration errors, and for scaffolded
/// commands.
pub const USAGE_ERROR: u8 = 2;

/// The exit status when a task fails, or the build does not settle.
pub const TASK_FAILED: u8 = 1;

/// The exit status of a build interrupted with Ctrl-C.
pub const INTERRUPTED: u8 = 130;

/// The width that parse errors and help are wrapped to.
const MESSAGE_WIDTH: usize = 100;

/// The error `kizz build --parallel` prints.
pub const PARALLEL_NOT_IMPLEMENTED: &str = "parallel execution is not implemented yet; \
                                            `kizz build` runs one task at a time with \
                                            --sequential, the default";

/// Global `kizz` options.
#[derive(Debug, Clone, Bpaf)]
#[bpaf(options, version)]
pub struct Cli {
    /// Color policy for terminal output.
    #[bpaf(long, argument("auto|always|never"), fallback(ColorMode::Auto))]
    pub color: ColorMode,

    /// Suppress the banner and other non-essential output.
    #[bpaf(long)]
    pub quiet: bool,

    /// Suppress the startup banner.
    #[bpaf(long)]
    pub no_banner: bool,

    /// Print more detail, such as cached commands and environment values.
    #[bpaf(short, long)]
    pub verbose: bool,

    /// The selected command.
    #[bpaf(external(command))]
    pub command: Command,
}

impl Cli {
    /// Run the selected command and return the process exit status.
    #[must_use]
    pub fn exec(self) -> ExitCode {
        self.color.apply();
        if !self.quiet && !self.no_banner {
            ui::print_banner();
        }

        match self.command {
            Command::Build {
                execution: Execution::Parallel,
                ..
            } => {
                ui::print_error(PARALLEL_NOT_IMPLEMENTED);
                ExitCode::from(USAGE_ERROR)
            },
            Command::Build {
                targets, explain, ..
            } => {
                crate::build::run(targets.as_deref(), crate::build::Options {
                    explain,
                    verbose: self.verbose,
                })
            },
            Command::Why { path } => crate::why::run(path.as_deref(), self.verbose),
            Command::List { targets } => crate::list::run(targets.as_deref()),
            Command::Cache(CacheCommand::Clean) => {
                stub(
                    "cache clean",
                    "Remove entries for files and dependencies that no longer exist.",
                    None,
                )
            },
            Command::Cache(CacheCommand::Reset { all, path, .. }) => {
                stub(
                    "cache reset",
                    "Wipe the cache entries under a path, or the whole cache with --all.",
                    if all {
                        Some("--all".to_owned())
                    } else {
                        path.map(|path| path.display().to_string())
                    },
                )
            },
            Command::Graph => stub("graph", "Show the learned task graph.", None),
            Command::Run => {
                stub(
                    "run",
                    "Run interactive or long-running commands without tracing or caching.",
                    None,
                )
            },
            Command::Image => stub("image", "Build container images.", None),
        }
    }
}

/// Print the scaffold message of a command and return the usage-error status.
fn stub(
    command_name: &str,
    summary: &str,
    target: Option<String>,
) -> ExitCode {
    ui::print_stub(command_name, summary, target);
    ExitCode::from(USAGE_ERROR)
}

/// Parse the process's command line, run the selected command, and return the exit
/// status.
///
/// Help and version requests exit with status 0, and every command-line error
/// exits with [`USAGE_ERROR`].
#[must_use]
pub fn run() -> ExitCode {
    match cli().run_inner(Args::current_args()) {
        Ok(parsed) => parsed.exec(),
        Err(failure) => {
            let status = parse_failure_status(&failure);
            failure.print_message(MESSAGE_WIDTH);
            ExitCode::from(status)
        },
    }
}

/// The exit status for a command line that did not parse into a command.
fn parse_failure_status(failure: &ParseFailure) -> u8 {
    match failure {
        ParseFailure::Stdout(..) | ParseFailure::Completion(_) => 0,
        ParseFailure::Stderr(_) => USAGE_ERROR,
    }
}

/// The selected command and its parsed arguments.
#[derive(Debug, Clone, Bpaf)]
pub enum Command {
    /// Run the tasks for the targets in the current directory and below.
    #[bpaf(command)]
    Build {
        /// Print the full reason for every command, cached ones included.
        #[bpaf(long)]
        explain: bool,

        /// How tasks are scheduled.
        #[bpaf(external(execution), fallback(Execution::Sequential))]
        execution: Execution,

        /// One target, or several joined with `+`, such as `ci+check`.
        #[bpaf(positional("TARGETS"))]
        targets: Option<String>,
    },

    /// Print why each command ran or was cached in the most recent build.
    #[bpaf(command)]
    Why {
        /// Show tasks under this path; the current directory by default.
        #[bpaf(positional("PATH"))]
        path: Option<PathBuf>,
    },

    /// List the discovered tasks with their file, line, name and targets.
    #[bpaf(command)]
    List {
        /// Show only the tasks of these targets, joined with `+`.
        #[bpaf(positional("TARGETS"))]
        targets: Option<String>,
    },

    /// Clean up or reset the shared cache.
    #[bpaf(command)]
    Cache(#[bpaf(external(cache_command))] CacheCommand),

    /// Show the learned task graph (not implemented yet).
    #[bpaf(command)]
    Graph,

    /// Run commands without tracing or caching (not implemented yet).
    #[bpaf(command)]
    Run,

    /// Build container images (not implemented yet).
    #[bpaf(command)]
    Image,
}

/// How `kizz build` schedules tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Bpaf)]
pub enum Execution {
    /// Run one task at a time (the default).
    #[bpaf(long)]
    Sequential,

    /// Run independent tasks in parallel (not implemented yet).
    #[bpaf(long)]
    Parallel,
}

/// The `kizz cache` commands.
#[derive(Debug, Clone, Bpaf)]
pub enum CacheCommand {
    /// Remove entries for files and dependencies that no longer exist.
    #[bpaf(command)]
    Clean,

    /// Wipe the entries under a path, or the whole cache with --all, after
    /// confirmation.
    #[bpaf(command)]
    Reset {
        /// Wipe the whole cache.
        #[bpaf(long)]
        all: bool,

        /// Skip the confirmation.
        #[bpaf(long)]
        yes: bool,

        /// Wipe the entries for tasks under this path; the current directory by
        /// default.
        #[bpaf(positional("PATH"))]
        path: Option<PathBuf>,
    },
}

/// Color policy for terminal output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    /// Use color when the output is a terminal that supports it.
    Auto,
    /// Always emit color escape sequences.
    Always,
    /// Never emit color escape sequences.
    Never,
}

impl ColorMode {
    /// Apply the policy to the terminal styling of standard output and standard
    /// error.
    fn apply(self) {
        let enabled = match self {
            Self::Auto => return,
            Self::Always => true,
            Self::Never => false,
        };
        console::set_colors_enabled(enabled);
        console::set_colors_enabled_stderr(enabled);
    }
}

impl fmt::Display for ColorMode {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Never => "never",
        })
    }
}

impl FromStr for ColorMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Self::Auto),
            "always" => Ok(Self::Always),
            "never" => Ok(Self::Never),
            other => Err(format!("unsupported color mode: {other}")),
        }
    }
}

/// Unit tests for command-line parsing.
#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{CacheCommand, Cli, ColorMode, Command, Execution, USAGE_ERROR, cli};

    /// Parse a command line that must be valid.
    fn parse(args: &[&str]) -> Cli {
        cli().run_inner(args).unwrap()
    }

    /// The parser satisfies bpaf's own consistency checks.
    #[test]
    fn parser_invariants_hold() {
        cli().check_invariants(false);
    }

    /// Global options parse, with their defaults.
    #[test]
    fn parses_global_options() {
        let defaults = parse(&["graph"]);
        assert_eq!(defaults.color, ColorMode::Auto);
        assert!(!defaults.quiet && !defaults.no_banner && !defaults.verbose);

        let set = parse(&["--color", "never", "--quiet", "--no-banner", "-v", "graph"]);
        assert_eq!(set.color, ColorMode::Never);
        assert!(set.quiet && set.no_banner && set.verbose);
        assert!(parse(&["--verbose", "run"]).verbose);
        assert_eq!(
            parse(&["--color", "always", "image"]).color,
            ColorMode::Always
        );
    }

    /// `kizz build` parses its targets and options, and is sequential by default.
    #[test]
    fn parses_build() {
        match parse(&["build", "ci+check"]).command {
            Command::Build {
                explain,
                execution,
                targets,
            } => {
                assert!(!explain);
                assert_eq!(execution, Execution::Sequential);
                assert_eq!(targets.as_deref(), Some("ci+check"));
            },
            other => panic!("expected build, got {other:?}"),
        }
        match parse(&["build", "--explain", "--parallel"]).command {
            Command::Build {
                explain,
                execution,
                targets,
            } => {
                assert!(explain);
                assert_eq!(execution, Execution::Parallel);
                assert_eq!(targets, None);
            },
            other => panic!("expected build, got {other:?}"),
        }
        assert!(matches!(
            parse(&["build", "--sequential", "ci"]).command,
            Command::Build {
                execution: Execution::Sequential,
                ..
            }
        ));
    }

    /// `kizz why`, `kizz list`, and the stubs parse.
    #[test]
    fn parses_other_commands() {
        assert!(matches!(parse(&["why"]).command, Command::Why {
            path: None
        }));
        match parse(&["why", "sub/dir"]).command {
            Command::Why { path } => assert_eq!(path, Some(PathBuf::from("sub/dir"))),
            other => panic!("expected why, got {other:?}"),
        }
        match parse(&["list", "ci"]).command {
            Command::List { targets } => assert_eq!(targets.as_deref(), Some("ci")),
            other => panic!("expected list, got {other:?}"),
        }
        assert!(matches!(parse(&["graph"]).command, Command::Graph));
        assert!(matches!(parse(&["run"]).command, Command::Run));
        assert!(matches!(parse(&["image"]).command, Command::Image));
    }

    /// `kizz cache clean` and `kizz cache reset` parse with their options.
    #[test]
    fn parses_cache_commands() {
        assert!(matches!(
            parse(&["cache", "clean"]).command,
            Command::Cache(CacheCommand::Clean)
        ));
        match parse(&["cache", "reset", "--all", "--yes"]).command {
            Command::Cache(CacheCommand::Reset { all, yes, path }) => {
                assert!(all && yes);
                assert_eq!(path, None);
            },
            other => panic!("expected cache reset, got {other:?}"),
        }
        match parse(&["cache", "reset", "project"]).command {
            Command::Cache(CacheCommand::Reset { all, yes, path }) => {
                assert!(!all && !yes);
                assert_eq!(path, Some(PathBuf::from("project")));
            },
            other => panic!("expected cache reset, got {other:?}"),
        }
    }

    /// Invalid command lines fail to parse with the usage-error status.
    #[test]
    fn rejects_invalid_command_lines() {
        for args in [
            &[][..],
            &["nonexistent"][..],
            &["--nonexistent", "graph"][..],
            &["build", "--nonexistent"][..],
            &["build", "--sequential", "--parallel"][..],
            &["--color", "sometimes", "graph"][..],
            &["cache"][..],
            &["cache", "nonexistent"][..],
        ] {
            let failure = cli().run_inner(args).unwrap_err();
            assert_eq!(
                super::parse_failure_status(&failure),
                USAGE_ERROR,
                "{args:?}"
            );
        }
        for args in [
            &["--help"][..],
            &["--version"][..],
            &["build", "--help"][..],
        ] {
            let failure = cli().run_inner(args).unwrap_err();
            assert_eq!(super::parse_failure_status(&failure), 0, "{args:?}");
        }
    }

    /// Color modes round-trip through their names.
    #[test]
    fn color_modes_round_trip() {
        for mode in [ColorMode::Auto, ColorMode::Always, ColorMode::Never] {
            assert_eq!(mode.to_string().parse::<ColorMode>(), Ok(mode));
        }
        assert!("sometimes".parse::<ColorMode>().is_err());
    }
}
