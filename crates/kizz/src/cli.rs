// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Command-line interface for the `kizz` tool.
//!
//! This module owns parsing and dispatch only. Every command is parsed with all of
//! its options; `kizz build` runs in the `build` module, `kizz list` in the `list`
//! module, `kizz why` in the `why` module, `kizz cache` in the `cache` module, and
//! `kizz execute` in the `execute` module, and the other commands are scaffolds
//! until later phases implement them. `kizz execute` never prints the banner.

use std::{ffi::OsString, fmt, path::PathBuf, process::ExitCode, str::FromStr};

use bpaf::{Args, Bpaf, ParseFailure, Parser};

use crate::ui;

/// The exit status for usage and configuration errors, and for scaffolded
/// commands.
pub const USAGE_ERROR: u8 = 2;

/// The exit status when a task fails, or the build does not settle.
pub const TASK_FAILED: u8 = 1;

/// The exit status of a build interrupted with Ctrl-C.
pub const INTERRUPTED: u8 = 130;

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
        let execute = matches!(self.command, Command::Execute { .. });
        if !self.quiet && !self.no_banner && !execute {
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
                targets,
                explain,
                cache_unknown_syscalls,
                fail_unchecked,
                ..
            } => {
                crate::build::run(targets.as_deref(), crate::build::Options {
                    explain,
                    verbose: self.verbose,
                    engine: crate::build::EngineOptions {
                        cache_unknown_syscalls,
                        fail_unchecked,
                    },
                })
            },
            Command::Why { path } => crate::why::run(path.as_deref(), self.verbose),
            Command::List { targets } => crate::list::run(targets.as_deref()),
            Command::Cache(CacheCommand::Clean) => crate::cache::clean(),
            Command::Cache(CacheCommand::Reset { all, yes, path }) => {
                crate::cache::reset(path.as_deref(), all, yes)
            },
            Command::Execute {
                output,
                compact,
                json,
                program,
            } => {
                crate::execute::run(crate::execute::Options {
                    output,
                    compact,
                    json,
                    program,
                })
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
/// exits with [`USAGE_ERROR`]. Their message is written through the `ui` module,
/// which drops a write that fails, such as one to a closed pipe, where `bpaf`'s
/// `ParseFailure::print_message` would panic in the `print!` macros.
#[must_use]
pub fn run() -> ExitCode {
    match cli().run_inner(Args::current_args()) {
        Ok(parsed) => parsed.exec(),
        Err(failure) => {
            let status = parse_failure_status(&failure);
            ui::print_parse_failure(&failure);
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

        /// Cache commands that made system calls the tracer does not know.
        #[bpaf(long)]
        cache_unknown_syscalls: bool,

        /// Fail a task that leaves a non-zero exit unchecked: no `expect`, and no
        /// read of `rc` or `signal`.
        #[bpaf(long)]
        fail_unchecked: bool,

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

    /// List the discovered tasks with their file, line, heading, targets and options.
    #[bpaf(command)]
    List {
        /// Show only the tasks of these targets, joined with `+`.
        #[bpaf(positional("TARGETS"))]
        targets: Option<String>,
    },

    /// Clean up or reset the shared cache.
    #[bpaf(command)]
    Cache(#[bpaf(external(cache_command))] CacheCommand),

    /// Run a program, trace its whole process tree, and write a report of what it
    /// touched.
    ///
    /// The program gets kizz's environment, working directory, standard streams and
    /// terminal, and kizz exits with its status: its exit code, or 128 plus the
    /// signal that killed it; 127 when it is not found, 126 when it cannot be run,
    /// 125 when kizz fails, and 2 for a command-line error. No kizz.md and no cache
    /// are needed or used.
    #[bpaf(command)]
    Execute {
        /// Write the observation report to this file, which is replaced once the
        /// program's whole process tree has ended; required, in an existing
        /// directory.
        #[bpaf(long, argument("FILE"))]
        output: PathBuf,

        /// Combine the observations of every process into one set, instead of a
        /// report for each process.
        #[bpaf(long)]
        compact: bool,

        /// Write JSON instead of readable text.
        #[bpaf(long)]
        json: bool,

        /// The program and its arguments, after the required `--`.
        #[bpaf(external(program_and_arguments))]
        program: Vec<OsString>,
    },

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

/// The program and its arguments of `kizz execute`: strict positional arguments,
/// which come only after `--`, at least the program.
fn program_and_arguments() -> impl Parser<Vec<OsString>> {
    bpaf::positional::<OsString>("PROGRAM")
        .help(
            "The program to run, after `--`, and its arguments; a name without `/` is looked \
             up in PATH",
        )
        .strict()
        .some("kizz execute needs `--` and then the program to run")
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
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]
mod tests {
    use std::{ffi::OsString, path::PathBuf};

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

    /// `kizz build` parses its targets and options, and is sequential by default;
    /// `--fail-unchecked` is off unless given.
    #[test]
    fn parses_build() {
        match parse(&["build", "ci+check"]).command {
            Command::Build {
                explain,
                cache_unknown_syscalls,
                fail_unchecked,
                execution,
                targets,
            } => {
                assert!(!explain && !cache_unknown_syscalls && !fail_unchecked);
                assert_eq!(execution, Execution::Sequential);
                assert_eq!(targets.as_deref(), Some("ci+check"));
            },
            other => panic!("expected build, got {other:?}"),
        }
        match parse(&["build", "--explain", "--parallel"]).command {
            Command::Build {
                explain,
                cache_unknown_syscalls,
                fail_unchecked,
                execution,
                targets,
            } => {
                assert!(explain && !cache_unknown_syscalls && !fail_unchecked);
                assert_eq!(execution, Execution::Parallel);
                assert_eq!(targets, None);
            },
            other => panic!("expected build, got {other:?}"),
        }
        match parse(&["build", "--cache-unknown-syscalls", "TARGET"]).command {
            Command::Build {
                explain,
                cache_unknown_syscalls,
                fail_unchecked,
                execution,
                targets,
            } => {
                assert!(!explain && cache_unknown_syscalls && !fail_unchecked);
                assert_eq!(execution, Execution::Sequential);
                assert_eq!(targets.as_deref(), Some("TARGET"));
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

    /// `kizz build --fail-unchecked TARGET` parses, with the option set and every
    /// other option at its default.
    #[test]
    fn parses_build_fail_unchecked() {
        match parse(&["build", "--fail-unchecked", "TARGET"]).command {
            Command::Build {
                explain,
                cache_unknown_syscalls,
                fail_unchecked,
                execution,
                targets,
            } => {
                assert!(fail_unchecked && !explain && !cache_unknown_syscalls);
                assert_eq!(execution, Execution::Sequential);
                assert_eq!(targets.as_deref(), Some("TARGET"));
            },
            other => panic!("expected build, got {other:?}"),
        }
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

    /// `kizz execute` parses `--output`, `--compact` and `--json`, and the program
    /// with its arguments after `--`, where words that look like options are
    /// arguments of the program.
    #[test]
    fn parses_execute() {
        let words =
            |words: &[&str]| -> Vec<OsString> { words.iter().map(OsString::from).collect() };
        match parse(&["execute", "--output=out.txt", "--", "make", "-j8", "--keep"]).command {
            Command::Execute {
                output,
                compact,
                json,
                program,
            } => {
                assert_eq!(output, PathBuf::from("out.txt"));
                assert!(!compact && !json);
                assert_eq!(program, words(&["make", "-j8", "--keep"]));
            },
            other => panic!("expected execute, got {other:?}"),
        }
        match parse(&[
            "--quiet",
            "execute",
            "--compact",
            "--json",
            "--output",
            "r.json",
            "--",
            "sh",
            "-c",
            "echo hi",
        ])
        .command
        {
            Command::Execute {
                output,
                compact,
                json,
                program,
            } => {
                assert_eq!(output, PathBuf::from("r.json"));
                assert!(compact && json);
                assert_eq!(program, words(&["sh", "-c", "echo hi"]));
            },
            other => panic!("expected execute, got {other:?}"),
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
            &["execute"][..],
            &["execute", "--", "make"][..],
            &["execute", "--output=out.txt"][..],
            &["execute", "--output=out.txt", "make"][..],
            &["execute", "--output=out.txt", "--"][..],
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
            &["execute", "--help"][..],
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
