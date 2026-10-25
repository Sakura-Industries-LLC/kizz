// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! End-to-end tests of the `kizz` command line, which run the built binary.
//!
//! Every run gets an explicit environment that holds only `KIZZ_CACHE_DIR`, set to
//! a temporary directory, and runs in another temporary directory, so no run can
//! reach the real cache or a real project.

use std::{io, process::Command};

/// A line of the banner that no other output contains.
const BANNER_MARKER: &str = "Copyright (c) 2026, Sakura Industries LLC";

/// The line every scaffolded command prints.
const STUB_MARKER: &str = "This command is parsed but not wired to behavior yet.";

/// The start of every terminal escape sequence.
const ESCAPE: &str = "\u{1b}[";

/// What one run of `kizz` produced.
#[derive(Debug)]
struct Run {
    /// The exit code, or `None` when a signal killed the process.
    status: Option<i32>,
    /// Standard output, with invalid UTF-8 replaced.
    stdout: String,
    /// Standard error, with invalid UTF-8 replaced.
    stderr: String,
}

/// Run the `kizz` binary with `args` and a temporary cache and working directory.
fn kizz(args: &[&str]) -> io::Result<Run> {
    let cache = tempfile::tempdir()?;
    let work = tempfile::tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_kizz"))
        .args(args)
        .env_clear()
        .env("KIZZ_CACHE_DIR", cache.path())
        .current_dir(work.path())
        .output()?;
    Ok(Run {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Every scaffolded command, with the name its scaffold message shows. `kizz build`,
/// `kizz list`, `kizz why`, and `kizz cache` are implemented, and `tests/build.rs`,
/// `tests/list.rs`, `tests/why.rs`, and `tests/cache.rs` cover them.
const STUBS: [(&[&str], &str); 3] = [
    (&["graph"], "graph"),
    (&["run"], "run"),
    (&["image"], "image"),
];

/// `kizz --help` lists every command and exits with status 0, and the help of the
/// commands with options lists them.
#[test]
fn help_lists_every_subcommand() {
    let run = kizz(&["--help"]).unwrap();
    assert_eq!(run.status, Some(0), "{run:?}");
    for command in ["build", "why", "list", "cache", "graph", "run", "image"] {
        assert!(
            run.stdout
                .lines()
                .any(|line| line.trim_start().starts_with(command)),
            "--help does not list {command}: {run:?}"
        );
    }
    for option in [
        "--color",
        "--quiet",
        "--no-banner",
        "--verbose",
        "--version",
    ] {
        assert!(
            run.stdout.contains(option),
            "--help lacks {option}: {run:?}"
        );
    }

    let cache = kizz(&["cache", "--help"]).unwrap();
    assert_eq!(cache.status, Some(0), "{cache:?}");
    assert!(
        cache.stdout.contains("clean") && cache.stdout.contains("reset"),
        "{cache:?}"
    );

    let build = kizz(&["build", "--help"]).unwrap();
    assert_eq!(build.status, Some(0), "{build:?}");
    for option in [
        "--explain",
        "--cache-unknown-syscalls",
        "--sequential",
        "--parallel",
        "TARGETS",
    ] {
        assert!(
            build.stdout.contains(option),
            "build --help lacks {option}: {build:?}"
        );
    }
    assert!(
        build.stdout.contains("does not know"),
        "build --help does not describe --cache-unknown-syscalls: {build:?}"
    );

    let reset = kizz(&["cache", "reset", "--help"]).unwrap();
    assert_eq!(reset.status, Some(0), "{reset:?}");
    for option in ["--all", "--yes", "PATH"] {
        assert!(
            reset.stdout.contains(option),
            "cache reset --help lacks {option}: {reset:?}"
        );
    }
}

/// `kizz --version` prints the version and exits with status 0.
#[test]
fn version_exits_successfully() {
    let run = kizz(&["--version"]).unwrap();
    assert_eq!(run.status, Some(0), "{run:?}");
    assert!(run.stdout.contains(env!("CARGO_PKG_VERSION")), "{run:?}");
}

/// Every command prints its scaffold message and exits with status 2.
#[test]
fn each_subcommand_prints_its_stub_message() {
    for (args, name) in STUBS {
        let run = kizz(args).unwrap();
        assert_eq!(run.status, Some(2), "{args:?}: {run:?}");
        assert!(
            run.stdout
                .contains(&format!("command scaffolded: {name}\n")),
            "{args:?}: {run:?}"
        );
        assert!(run.stdout.contains(STUB_MARKER), "{args:?}: {run:?}");
        assert_eq!(run.stderr, "", "{args:?}: {run:?}");
    }
}

/// A scaffold message says that its command was given no target.
#[test]
fn stub_messages_name_no_target() {
    for (args, _) in STUBS {
        let run = kizz(args).unwrap();
        assert!(run.stdout.contains("target: <none>\n"), "{args:?}: {run:?}");
    }
}

/// `kizz build --parallel` fails with the not-implemented error and status 2,
/// without running the build.
#[test]
fn build_parallel_fails_with_not_implemented_error() {
    for args in [
        &["build", "--parallel"][..],
        &["build", "--parallel", "--explain", "ci"][..],
    ] {
        let run = kizz(args).unwrap();
        assert_eq!(run.status, Some(2), "{args:?}: {run:?}");
        assert!(
            run.stderr
                .contains("error: parallel execution is not implemented yet"),
            "{args:?}: {run:?}"
        );
        assert!(
            !run.stdout.contains("command scaffolded"),
            "{args:?}: {run:?}"
        );
    }
}

/// Unknown commands and options, invalid option values, conflicting options, and a
/// missing command are usage errors with status 2.
#[test]
fn command_line_errors_exit_with_status_2() {
    for args in [
        &[][..],
        &["nonexistent"][..],
        &["--nonexistent", "graph"][..],
        &["build", "--nonexistent"][..],
        &["build", "--sequential", "--parallel"][..],
        &["--color", "sometimes", "graph"][..],
        &["cache"][..],
        &["cache", "nonexistent"][..],
        &["cache", "reset", "--all", "sub"][..],
        &["graph", "extra"][..],
    ] {
        let run = kizz(args).unwrap();
        assert_eq!(run.status, Some(2), "{args:?}: {run:?}");
        assert_ne!(run.stderr, "", "{args:?}: {run:?}");
        assert!(
            !run.stdout.contains("command scaffolded"),
            "{args:?}: {run:?}"
        );
    }
}

/// The banner is shown by default and suppressed by `--quiet` or `--no-banner`.
#[test]
fn quiet_and_no_banner_suppress_the_banner() {
    let shown = kizz(&["graph"]).unwrap();
    assert!(shown.stdout.contains(BANNER_MARKER), "{shown:?}");
    assert!(
        shown.stdout.contains(env!("CARGO_PKG_VERSION")),
        "{shown:?}"
    );

    for flag in ["--quiet", "--no-banner"] {
        let run = kizz(&[flag, "graph"]).unwrap();
        assert_eq!(run.status, Some(2), "{flag}: {run:?}");
        assert!(!run.stdout.contains(BANNER_MARKER), "{flag}: {run:?}");
        assert!(run.stdout.contains(STUB_MARKER), "{flag}: {run:?}");
    }
}

/// `--color never` output has no escape sequences, and `--color always` output is
/// styled, on both standard output and standard error.
#[test]
fn color_option_controls_styling() {
    let plain = kizz(&["--color", "never", "build", "ci"]).unwrap();
    assert!(plain.stdout.contains(BANNER_MARKER), "{plain:?}");
    assert!(!plain.stdout.contains(ESCAPE), "{plain:?}");
    let plain_error = kizz(&["--color", "never", "build", "--parallel"]).unwrap();
    assert!(plain_error.stderr.contains("error:"), "{plain_error:?}");
    assert!(!plain_error.stderr.contains(ESCAPE), "{plain_error:?}");

    let styled = kizz(&["--color", "always", "build", "ci"]).unwrap();
    assert!(styled.stdout.contains(ESCAPE), "{styled:?}");
    let quiet_styled = kizz(&["--color", "always", "--quiet", "graph"]).unwrap();
    assert!(quiet_styled.stdout.contains(ESCAPE), "{quiet_styled:?}");
    let styled_error = kizz(&["--color", "always", "build", "--parallel"]).unwrap();
    assert!(styled_error.stderr.contains(ESCAPE), "{styled_error:?}");
}
