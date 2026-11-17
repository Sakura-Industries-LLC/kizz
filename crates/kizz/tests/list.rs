// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Golden tests of `kizz list`, which run the built binary in fixture trees that the
//! tests write into temporary directories.
//!
//! Every run gets an explicit environment that holds only `KIZZ_CACHE_DIR`, set to a
//! temporary directory that must still be empty afterwards, because `kizz list` does
//! not use the cache.

#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "test code: the tests assert, lock and print"
)]

use std::{fs, io, os::unix::fs::symlink, path::Path, process::Command};

/// The start of every terminal escape sequence.
const ESCAPE: &str = "\u{1b}[";

/// The global options of every golden run: no color and no banner.
const PLAIN: [&str; 3] = ["--color", "never", "--no-banner"];

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

/// Run the `kizz` binary in `dir` with `args` and a temporary cache, and fail when
/// the run wrote to the cache.
fn kizz(
    dir: &Path,
    args: &[&str],
) -> io::Result<Run> {
    let cache = tempfile::tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_kizz"))
        .args(args)
        .env_clear()
        .env("KIZZ_CACHE_DIR", cache.path())
        .current_dir(dir)
        .output()?;
    if fs::read_dir(cache.path())?.next().is_some() {
        return Err(io::Error::other(format!(
            "kizz {args:?} wrote to the cache"
        )));
    }
    Ok(Run {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Run `kizz --color never --no-banner list` with `targets` in `dir`.
fn list(
    dir: &Path,
    targets: &[&str],
) -> io::Result<Run> {
    let mut args = PLAIN.to_vec();
    args.push("list");
    args.extend_from_slice(targets);
    kizz(dir, &args)
}

/// Write `content` to `path` under `root`, creating its directories.
fn write(
    root: &Path,
    path: &str,
    content: &str,
) -> io::Result<()> {
    let path = root.join(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)
}

/// The top-level `kizz.md` of the fixture: two tasks with backtick and tilde fences,
/// a `starlark` block without targets, and a `starlark` block nested in another
/// fenced block.
const TOP: &str = "# Project

Run `kizz build ci`.

## Build the library

```starlark ci check
run(\"make\")
```

## Notes

```starlark
x = 1
```

````markdown
```starlark ci
nested = True
```
````

~~~starlark lint
run(\"lint\")
~~~
";

/// Build the fixture tree in `root`:
///
/// * `kizz.md`, from [`TOP`];
/// * `.hidden/kizz.md`, in a hidden directory;
/// * `ignored/kizz.md`, which Git and ignore files exclude, with a `.git` directory so
///   the rules would apply;
/// * `a/kizz.md` and `a/b/kizz.md`, a parent and its subdirectory;
/// * `real/kizz.md`, also reachable as `alias/kizz.md` through `alias -> real`, which
///   comes first in the default order;
/// * `zz -> a/b`, a second path to `a/b/kizz.md`, which comes later;
/// * `b/kizz.md -> ../a/kizz.md`, a second path to a file;
/// * `a/up -> ..`, a link that would recurse into its own parent;
/// * `dangling -> missing`, a link that leads nowhere and hides no `kizz.md`;
/// * `README.md` and `a/notes.md`, other Markdown that is never read.
fn fixture(root: &Path) -> io::Result<()> {
    write(root, "kizz.md", TOP)?;
    write(root, ".hidden/kizz.md", "# Hidden\n\n```starlark ci\n```\n")?;
    write(root, ".git/HEAD", "ref: refs/heads/main\n")?;
    write(root, ".gitignore", "ignored/\nkizz.md\n")?;
    write(root, ".ignore", "ignored/\n")?;
    write(
        root,
        "ignored/kizz.md",
        "# Ignored\n\n```starlark docs\n```\n",
    )?;
    write(root, "a/kizz.md", "# A\n\n```starlark check\n```\n")?;
    write(root, "a/b/kizz.md", "# B\n\n```starlark ci\n```\n")?;
    write(root, "real/kizz.md", "# Real\n\n```starlark release\n```\n")?;
    write(root, "README.md", "```starlark ci\n```\n")?;
    write(root, "a/notes.md", "```starlark ci\n```\n")?;
    symlink("real", root.join("alias"))?;
    symlink("a/b", root.join("zz"))?;
    fs::create_dir(root.join("b"))?;
    symlink("../a/kizz.md", root.join("b/kizz.md"))?;
    symlink("..", root.join("a/up"))?;
    symlink("missing", root.join("dangling"))
}

/// The warnings of the fixture, as `kizz list` prints them on standard error: each
/// one as it is found, then all of them again in the final summary.
const FIXTURE_STDERR: &str = "\
warning: skipped the symbolic link a/up: it leads back to ., one of its own parent \
directories, so following it would recurse
warning: kizz.md:13 (Notes): this starlark block has neither targets nor a name, so it is not \
a task
2 warnings:
  skipped the symbolic link a/up: it leads back to ., one of its own parent directories, \
so following it would recurse
  kizz.md:13 (Notes): this starlark block has neither targets nor a name, so it is not a task
";

/// `kizz list` shows every task in the default order, from hidden directories and
/// from `kizz.md` files that Git ignores too, each `kizz.md` once under its first
/// path, with every warning loud and repeated in the summary.
#[test]
fn lists_every_task_with_warnings() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path()).unwrap();

    let run = list(dir.path(), &[]).unwrap();
    assert_eq!(run.status, Some(0), "{run:?}");
    assert_eq!(
        run.stdout,
        "\
kizz.md:7 (Build the library) [ci, check]
kizz.md:23 (Notes) [lint]
.hidden/kizz.md:3 (Hidden) [ci]
a/kizz.md:3 (A) [check]
a/b/kizz.md:3 (B) [ci]
alias/kizz.md:3 (Real) [release]
ignored/kizz.md:3 (Ignored) [docs]
7 tasks in 6 kizz.md files.
",
        "{run:?}"
    );
    assert_eq!(run.stderr, FIXTURE_STDERR, "{run:?}");

    // The order is stable across runs of the parallel walk.
    for _ in 0..5 {
        let again = list(dir.path(), &[]).unwrap();
        assert_eq!(again.stdout, run.stdout, "{again:?}");
        assert_eq!(again.stderr, run.stderr, "{again:?}");
    }
}

/// A union lists the tasks of any of its targets, in the default order, and a task
/// with several selected targets once; discovery warnings still show.
#[test]
fn lists_the_tasks_a_union_selects() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path()).unwrap();

    let union = list(dir.path(), &["ci+check"]).unwrap();
    assert_eq!(union.status, Some(0), "{union:?}");
    assert_eq!(
        union.stdout,
        "\
kizz.md:7 (Build the library) [ci, check]
.hidden/kizz.md:3 (Hidden) [ci]
a/kizz.md:3 (A) [check]
a/b/kizz.md:3 (B) [ci]
4 of 7 tasks selected by `ci+check`.
",
        "{union:?}"
    );
    assert_eq!(union.stderr, FIXTURE_STDERR, "{union:?}");

    let one = list(dir.path(), &["release"]).unwrap();
    assert_eq!(one.status, Some(0), "{one:?}");
    assert_eq!(
        one.stdout, "alias/kizz.md:3 (Real) [release]\n1 of 7 tasks selected by `release`.\n",
        "{one:?}"
    );

    let none = list(dir.path(), &["missing"]).unwrap();
    assert_eq!(none.status, Some(0), "{none:?}");
    assert_eq!(
        none.stdout, "0 of 7 tasks selected by `missing`.\n",
        "{none:?}"
    );
}

/// Malformed target expressions are usage errors with status 2.
#[test]
fn malformed_expressions_are_usage_errors() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "kizz.md", "```starlark ci\n```\n").unwrap();
    for (expression, message) in [
        (
            "ci+",
            "error: invalid target expression `ci+`: a target is missing; join targets with \
             `+`, such as `ci+check`\n",
        ),
        (
            "+",
            "error: invalid target expression `+`: a target is missing; join targets with \
             `+`, such as `ci+check`\n",
        ),
        (
            "",
            "error: invalid target expression ``: a target is missing; join targets with \
             `+`, such as `ci+check`\n",
        ),
        (
            "ci!",
            "error: invalid target expression `ci!`: invalid target name `ci!`: target names \
             use only letters, digits, `_`, `.` and `-`\n",
        ),
        (
            "ci+a/b",
            "error: invalid target expression `ci+a/b`: invalid target name `a/b`: target \
             names use only letters, digits, `_`, `.` and `-`\n",
        ),
    ] {
        let run = list(dir.path(), &[expression]).unwrap();
        assert_eq!(run.status, Some(2), "{expression:?}: {run:?}");
        assert_eq!(run.stdout, "", "{expression:?}: {run:?}");
        assert_eq!(run.stderr, message, "{expression:?}: {run:?}");
    }
}

/// Invalid target names in a `kizz.md` are configuration errors with status 2 that
/// name the target, file, line, and heading.
#[test]
fn invalid_target_names_are_configuration_errors() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "kizz.md",
        "# Release\n\n```starlark ci ship!\n```\n",
    )
    .unwrap();
    write(dir.path(), "sub/kizz.md", "```starlark a+b\n```\n").unwrap();
    let run = list(dir.path(), &[]).unwrap();
    assert_eq!(run.status, Some(2), "{run:?}");
    assert_eq!(run.stdout, "", "{run:?}");
    assert_eq!(
        run.stderr,
        "\
error: kizz.md:3 (Release): invalid target name `ship!`: target names use only letters, \
digits, `_`, `.` and `-`
error: sub/kizz.md:1: invalid target name `a+b`: target names use only letters, digits, \
`_`, `.` and `-`
",
        "{run:?}"
    );
}

/// The standard error of `kizz list` when loading finds `errors` along with two
/// warnings: a recursive link `dir/up -> ..`, and a block without targets at
/// `file_and_heading`. Each warning shows as it is found, then the errors, then every
/// warning again in the final summary.
fn stderr_with_errors(
    dir: &str,
    file_and_heading: &str,
    errors: &str,
) -> String {
    let link = format!(
        "skipped the symbolic link {dir}/up: it leads back to ., one of its own parent \
         directories, so following it would recurse"
    );
    let untagged = format!(
        "{file_and_heading}: this starlark block has neither targets nor a name, so it is not a \
         task"
    );
    format!("warning: {link}\nwarning: {untagged}\n{errors}2 warnings:\n  {link}\n  {untagged}\n")
}

/// Configuration errors still show every discovery and parsing warning, loudly and
/// again in the final summary: a recursive link and a block without targets, with an
/// invalid target in the same file. The status stays 2 and no task is listed.
#[test]
fn invalid_targets_keep_the_warnings() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "kizz.md",
        "# Notes\n\n```starlark\n```\n\n```starlark ci bad!\n```\n",
    )
    .unwrap();
    write(root, "a/kizz.md", "# A\n\n```starlark check\n```\n").unwrap();
    symlink("..", root.join("a/up")).unwrap();
    let expected = stderr_with_errors(
        "a",
        "kizz.md:3 (Notes)",
        "error: kizz.md:6 (Notes): invalid target name `bad!`: target names use only \
         letters, digits, `_`, `.` and `-`\n",
    );
    let selections: [&[&str]; 2] = [&[], &["check"]];
    for targets in selections {
        let run = list(root, targets).unwrap();
        assert_eq!(run.status, Some(2), "{targets:?}: {run:?}");
        assert_eq!(run.stdout, "", "{targets:?}: {run:?}");
        assert_eq!(run.stderr, expected, "{targets:?}: {run:?}");
    }

    let loud = kizz(root, &["--color", "always", "--no-banner", "list"]).unwrap();
    assert_eq!(loud.status, Some(2), "{loud:?}");
    assert_eq!(loud.stdout, "", "{loud:?}");
    for warning in ["a/up", "kizz.md:3 (Notes)"] {
        let styled = loud
            .stderr
            .lines()
            .filter(|line| line.contains(ESCAPE) && line.contains(warning))
            .count();
        assert_eq!(styled, 2, "{warning}: {loud:?}");
    }
    assert!(loud.stderr.contains("2 warnings:"), "{loud:?}");
}

/// A `kizz.md` that cannot be read is a configuration error that still shows every
/// warning from discovery and the other files, and repeats them in the summary.
#[test]
fn unreadable_files_keep_the_warnings() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("kizz.md"), [0xFF, 0xFE, 0x00]).unwrap();
    write(root, "sub/kizz.md", "# Sub\n\n```starlark\n```\n").unwrap();
    symlink("..", root.join("sub/up")).unwrap();
    let run = list(root, &[]).unwrap();
    assert_eq!(run.status, Some(2), "{run:?}");
    assert_eq!(run.stdout, "", "{run:?}");
    assert_eq!(
        run.stderr,
        stderr_with_errors(
            "sub",
            "sub/kizz.md:3 (Sub)",
            "error: could not read kizz.md: stream did not contain valid UTF-8\n",
        ),
        "{run:?}"
    );
}

/// A directory without `kizz.md` files lists no tasks and no warnings.
#[test]
fn an_empty_directory_lists_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let run = list(dir.path(), &[]).unwrap();
    assert_eq!(run.status, Some(0), "{run:?}");
    assert_eq!(run.stdout, "0 tasks in 0 kizz.md files.\n", "{run:?}");
    assert_eq!(run.stderr, "", "{run:?}");
}

/// A `kizz.md` that is a dangling symbolic link cannot be read, so it gets a warning,
/// while other dangling links hide no `kizz.md` and get none.
#[test]
fn a_dangling_kizz_md_link_warns() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "kizz.md", "# Top\n\n```starlark ci\n```\n").unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    symlink("missing.md", dir.path().join("sub/kizz.md")).unwrap();
    symlink("missing", dir.path().join("sub/other")).unwrap();
    let run = list(dir.path(), &[]).unwrap();
    assert_eq!(run.status, Some(0), "{run:?}");
    assert_eq!(
        run.stdout, "kizz.md:3 (Top) [ci]\n1 task in 1 kizz.md file.\n",
        "{run:?}"
    );
    assert_eq!(
        run.stderr,
        "\
warning: could not read sub/kizz.md: No such file or directory (os error 2); any kizz.md in or \
behind it was not found
1 warning:
  could not read sub/kizz.md: No such file or directory (os error 2); any kizz.md in or behind \
it was not found
",
        "{run:?}"
    );
}

/// `kizz list` starts in the current directory and never looks above it.
#[test]
fn lists_only_the_current_directory_and_below() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "kizz.md", "# Top\n\n```starlark ci\n```\n").unwrap();
    write(dir.path(), "sub/kizz.md", "# Sub\n\n```starlark ci\n```\n").unwrap();
    write(
        dir.path(),
        "sub/deeper/kizz.md",
        "# Deeper\n\n```starlark ci\n```\n",
    )
    .unwrap();
    write(
        dir.path(),
        "other/kizz.md",
        "# Other\n\n```starlark ci\n```\n",
    )
    .unwrap();
    let run = list(&dir.path().join("sub"), &[]).unwrap();
    assert_eq!(run.status, Some(0), "{run:?}");
    assert_eq!(
        run.stdout,
        "kizz.md:3 (Sub) [ci]\ndeeper/kizz.md:3 (Deeper) [ci]\n2 tasks in 2 kizz.md files.\n",
        "{run:?}"
    );
    assert_eq!(run.stderr, "", "{run:?}");
}

/// Warnings are bold and colored with `--color always`, and the banner shows without
/// `--no-banner`.
#[test]
fn warnings_are_loud() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "kizz.md", "# Notes\n\n```starlark\n```\n").unwrap();
    let run = kizz(dir.path(), &["--color", "always", "list"]).unwrap();
    assert_eq!(run.status, Some(0), "{run:?}");
    assert!(run.stdout.contains("Sakura Industries LLC"), "{run:?}");
    let styled_warnings = run
        .stderr
        .lines()
        .filter(|line| line.contains(ESCAPE) && line.contains("kizz.md:3 (Notes)"))
        .count();
    assert_eq!(styled_warnings, 2, "{run:?}");
    assert!(run.stderr.contains("1 warning:"), "{run:?}");
}

/// A `kizz.md` with task options: a task with targets and both options, a named
/// task without targets, a task with only `cache=never` besides its target, and a
/// block with only `cache=never`, which is untagged.
const OPTIONS: &str = "# Options

## Lint

```starlark cache=never ci name=lint check
run(\"lint\")
```

## Deploy

```starlark name=deploy
run(\"ship\")
```

## Fresh

```starlark ci cache=never
run(\"date\")
```

## Odd

```starlark cache=never
```
";

/// The warnings of [`OPTIONS`], as `kizz list` prints them on standard error.
const OPTIONS_STDERR: &str = "\
warning: kizz.md:23 (Odd): this starlark block has neither targets nor a name, so it is not a \
task
1 warning:
  kizz.md:23 (Odd): this starlark block has neither targets nor a name, so it is not a task
";

/// `kizz list` shows each task's options in their info-string form after its
/// targets, and a named task without targets with `[]`; a target expression never
/// selects a task without targets.
#[test]
fn lists_task_options_and_named_tasks_without_targets() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "kizz.md", OPTIONS).unwrap();

    let all = list(dir.path(), &[]).unwrap();
    assert_eq!(all.status, Some(0), "{all:?}");
    assert_eq!(
        all.stdout,
        "\
kizz.md:5 (Lint) [ci, check] name=lint cache=never
kizz.md:11 (Deploy) [] name=deploy
kizz.md:17 (Fresh) [ci] cache=never
3 tasks in 1 kizz.md file.
",
        "{all:?}"
    );
    assert_eq!(all.stderr, OPTIONS_STDERR, "{all:?}");

    let ci = list(dir.path(), &["ci"]).unwrap();
    assert_eq!(ci.status, Some(0), "{ci:?}");
    assert_eq!(
        ci.stdout,
        "\
kizz.md:5 (Lint) [ci, check] name=lint cache=never
kizz.md:17 (Fresh) [ci] cache=never
2 of 3 tasks selected by `ci`.
",
        "{ci:?}"
    );
    assert_eq!(ci.stderr, OPTIONS_STDERR, "{ci:?}");

    let named = list(dir.path(), &["deploy"]).unwrap();
    assert_eq!(named.status, Some(0), "{named:?}");
    assert_eq!(
        named.stdout, "0 of 3 tasks selected by `deploy`.\n",
        "{named:?}"
    );
}

/// Mistakes in task options are configuration errors with status 2 that name the
/// file, line, heading and option; a name given to two tasks of one file is one too.
#[test]
fn task_option_mistakes_are_configuration_errors() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "kizz.md",
        "# Opts\n\n```starlark ci color=red\n```\n\n```starlark name=x\n```\n\n\
         ```starlark ci name=x\n```\n",
    )
    .unwrap();
    let run = list(dir.path(), &[]).unwrap();
    assert_eq!(run.status, Some(2), "{run:?}");
    assert_eq!(run.stdout, "", "{run:?}");
    assert_eq!(
        run.stderr,
        "\
error: kizz.md:3 (Opts): unknown task option `color=red`: the task options are `name=<name>` \
and `cache=never`
error: kizz.md:9 (Opts): task option `name=x` gives a name that the task at line 6 of this \
file already has, and a name is unique within its kizz.md
",
        "{run:?}"
    );
}
