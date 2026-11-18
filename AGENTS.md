# Agent guide for kizz

Kizz is a Linux build engine written in Rust. [architecture.md](architecture.md) is the approved spec: it records every decision,
and its "Implementation plan" section defines the phases in order.
Implement it as written, and raise anything it leaves open instead of guessing.

## Quality gate

[kizz.md](kizz.md) defines the build tasks.
Run `kizz build fix+ci` from the worktree after your last edit.
It must pass.
Use `./bootstrap.sh` to build and install kizz and its pinned tools on a new checkout.

* `kizz run sort-dictionary` sorts `.config/dictionaries/project.dic` case-insensitively and removes duplicates.
* `kizz build fix` runs the automated fixers.
* `kizz build ci` checks dictionary order and formatting, runs lints and tests, builds docs,
  checks dependency policy and builds release binaries.

Cargo can refresh `Cargo.lock` during builds, and `Cargo.lock` is committed.
Tasks can load settings from the optional root `.env`.
It holds one machine's settings, so never commit it.
If a seccomp filter refuses calls before kizz's tracer sees them, `KIZZ_TEST_SKIP_BLOCKED_SYSCALLS=1` there skips their checks.

## Rules

* Every Rust source file, `build.rs` and tests included, starts with this header, above any `//!` documentation:
  `// Copyright (c) 2026 Sakura Industries LLC.`, then `//`, then `// SPDX-License-Identifier: AGPL-3.0-only`.
* Add spelling words only by appending lines to `.config/dictionaries/project.dic`, one canonical form per line.
  Never sort or rewrite the file by hand; the build task sorts it and removes duplicates.
* Tests never use the real cache: every test that runs the engine
  or the `kizz` binary sets `KIZZ_CACHE_DIR` to a temporary directory and gives each child process an explicit environment.
* The documentation site in `docs/` is a Zola site, whose pages link to each other with `@/` links.
  Its build output, `docs/public/`, is ignored by Git.
