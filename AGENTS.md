# Agent guide for kizz

Kizz is a Linux build engine written in Rust. [architecture.md](architecture.md) is the approved spec: it records every decision,
and its "Implementation plan" section defines the phases in order.
Implement it as written, and raise anything it leaves open instead of guessing.

## Recipes

The Justfile holds the commands, written out directly; kizz does not use Moon.

* `just sort-dictionary` sorts `.config/dictionaries/project.dic` case-insensitively and removes duplicate words.
* `just fix` runs every automated fixer: it sorts the dictionary, then runs the formatters and anything else
  that can be fixed automatically.
* `just check` checks formatting, lints, tests, docs, Markdown and spelling, and changes nothing.
* `just check-ci` checks and builds: it sorts the dictionary, runs the checks, checks licenses, and builds the release binaries.
  No recipe makes Cargo enforce an up-to-date lock file, so Cargo creates and refreshes `Cargo.lock` as needed,
  and `Cargo.lock` is committed.
* `just fix-ci` runs `fix` and then `check-ci`.

`just fix-ci` is the quality gate: run it from the worktree after your last edit, and it must pass.

The Justfile loads a `.env` file beside it if there is one; it holds one machine's settings, so never commit it.
If a seccomp filter refuses calls before kizz's tracer sees them, `KIZZ_TEST_SKIP_BLOCKED_SYSCALLS=1` there skips their checks.

## Rules

* Every Rust source file, `build.rs` and tests included, starts with this header, above any `//!` documentation:
  `// Copyright (c) 2026 Sakura Industries LLC.`, then `//`, then `// SPDX-License-Identifier: AGPL-3.0-only`.
* Add spelling words only by appending lines to `.config/dictionaries/project.dic`, one canonical form per line.
  Never sort or rewrite the file by hand; the recipes sort it and remove duplicates.
* Tests never use the real cache: every test that runs the engine or the `kizz` binary sets `KIZZ_CACHE_DIR`
  to a temporary directory and gives each child process an explicit environment.
