+++
weight = 1
+++

# Phase 9 measurements

This report covers phase 9 of the version 1 plan in
[architecture.md](https://github.com/Sakura-Industries-LLC/kizz/blob/main/architecture.md): tracing overhead and cache-check time on
real projects, discovery time on large trees, cache cleanup on a large cache, and the decision on a tracer fast path.
It ends with the defect the measurements found and fixed, and the follow-up work.

## Summary

* Tracing makes a compile-heavy build 15 to 25 percent slower while the command runs,
  and a no-op build that only checks file metadata about twice as slow.
  The rest of a `kizz build` run, from starting to recording and the confirming pass, adds 32 to 176 milliseconds.
* A cache check of an unchanged step takes 13 to 45 milliseconds for steps with up to 8,485 recorded paths,
  well below the untraced build it replaces.
* Discovery walks 102,551 directories and 402,500 files and finds 2,500 `kizz.md` files in 0.145 seconds.
* `kizz cache clean` checks 50,103 recorded paths of 50,000 step records in 0.61 seconds.
* The tracer does not need a fast path for version 1 (see [The fast-path decision](#the-fast-path-decision)).
* A cache check of tens of thousands of small steps spends most of its time in thread-pool overhead,
  9 seconds for 50,000 cached steps; that is the first follow-up item.
* One defect stopped a measurement: a command that read `/dev/fd/3` was never cached.
  It is fixed, with regression tests (see [Defect fixed in this phase](#defect-fixed-in-this-phase)).

## Environment

* Machine: AMD Ryzen 9 5950X, 16 cores and 32 threads, 126 GiB of memory.
* Kernel: Linux 7.2.11-1-cachyos, x86_64, with `kernel.yama.ptrace_scope` set to 1.
* File system of every copy, tree, and cache: ZFS, under `/tmp`.
* Kizz: the release binary `target/release/kizz` that `just fix-ci` built in the task worktree,
  from commit `e9e00bd69ee258d4a4c527ef101755e7dd75951f` plus the fix described below.
* The build tools are those on the caller's `PATH`; their versions were not inspected.
* Every run of `kizz` used `KIZZ_CACHE_DIR` set to a fresh temporary directory, never the user's cache.
* Times are wall-clock seconds from the `bash` `time` keyword, the median of five runs unless stated;
  the raw results also give user and system time.
* All work was under `/tmp/kizz-phase9-lIEBLy`, the measurement root, below called `$ROOT`.

## Measured projects

| Project | Source repository | Commit | Copies |
| --- | --- | --- | --- |
| tree-sitter-cddl | `/repo/tree-sitter-cddl`, branch `main` | `3c19b3fdf95f6582ab6c01d9881394f1b04ea427` | `$ROOT/measure/ts-*` |
| dntls-tunnel (Go) | `/repo/DNTLS/dntls-tunnel`, branch `main` | `3a50e2e8aa84d9d57460545959c4c6a57055c5d0` | `$ROOT/measure/go-*` |
| cosmos-sdk (discovery only) | `/repo/DNTLS/cosmos-sdk` | `2086680ff8b08fd269ee653e087ea577bab79534` | `$ROOT/discovery/cosmos-sdk` |

Every copy is the committed tree of the repository's `HEAD`, written into a new temporary directory with `git archive`,
so the original repositories were only read.
`git status --porcelain=v1` was empty in each of them before and after the measurements.
The Go module dntls-tunnel has 256 Go files and external dependencies such as `quic-go`, and `go build ./...` compiles all of them.
The tree-sitter-cddl repository has two builds: `make` compiles the C parser into a library,
and `cargo build --offline` builds the Rust binding, which compiles the same parser through the `cc` crate.

### Task files

The `kizz.md` files exist only in the copies.
The tree-sitter-cddl copies used this one:

````markdown
# tree-sitter-cddl

## Build the C library

```starlark build
env.inherit("PATH")
run("make", expect = [0])
```

## Build the Rust binding

```starlark rust
env.inherit("PATH", "HOME")
run("cargo", "build", "--offline", expect = [0])
```

## Build the C library, timed inside the trace

```starlark build-timed
env.inherit("PATH")
run("bash", "-c", "TIMEFORMAT='inner %R %U %S'; time make", expect = [0], stream = True)
```

## Build the Rust binding, timed inside the trace

```starlark rust-timed
env.inherit("PATH", "HOME")
run("bash", "-c", "TIMEFORMAT='inner %R %U %S'; time cargo build --offline", expect = [0], stream = True)
```
````

The dntls-tunnel copies used this one, with `MEASURE_GOCACHE` naming a fresh Go build cache under `$ROOT`:

````markdown
# dntls-tunnel

## Build every package

```starlark build
env.inherit("PATH", "HOME")
env.set("GOCACHE", env.caller("MEASURE_GOCACHE"))
env.set("GOPROXY", "off")
env.set("GOTOOLCHAIN", "local")
run("go", "build", "./...", expect = [0])
```

## Build every package, timed inside the trace

```starlark build-timed
env.inherit("PATH", "HOME")
env.set("GOCACHE", env.caller("MEASURE_GOCACHE"))
env.set("GOPROXY", "off")
env.set("GOTOOLCHAIN", "local")
run("bash", "-c", "TIMEFORMAT='inner %R %U %S'; time go build ./...", expect = [0], stream = True)
```
````

The fixed cost of `kizz` was measured with a task that runs only `true`:

````markdown
# No-op

## Run `true`

```starlark noop
env.inherit("PATH")
run("true", expect = [0])
```
````

## Commands

The scripts under `$ROOT` ran every measurement; these are their commands, with `$PATH` and `$HOME` the caller's values.
`$KIZZ` is `/repo/worktrees/kizz/core-builder-v1/target/release/kizz`.

A copy of a repository:

```bash
mkdir -p "$COPY" && git -C "$REPO" archive --format=tar HEAD | tar -x -C "$COPY"
```

Every timed command ran as `{ time COMMAND >LOG 2>&1; } 2>TIMES` with `TIMEFORMAT='%R %U %S'`.
Every traced run started `kizz` in the copy with only these variables:

```bash
cd "$COPY" && env -i PATH="$PATH" HOME="$HOME" KIZZ_CACHE_DIR="$CACHE" MEASURE_GOCACHE="$GOCACHE" \
    "$KIZZ" --no-banner build TARGET
```

Cold traced runs used a fresh copy and a fresh `$CACHE` each; the cache checks ran the same command again.
For a check after a change of metadata only, every committed file of the copy was touched, three seconds passed,
and the check ran twice:

```bash
(cd "$COPY" && git -C "$REPO" ls-files -z | xargs -0 touch) && sleep 3
```

`kizz cache clean` ran once after the checks, in the copy, to count the recorded paths.

The scripts, all under `$ROOT`, are:

* `lib.sh`, the helpers for copies, timing, baselines, and `kizz` runs shown above;
* `measure.sh`, the tracing and cache-check measurements of both projects;
* `discovery.sh 5 disc`, which runs `find TREE -name kizz.md` and then `kizz list` in each tree, five times;
* `cleanup.sh 50 1000 large`, the large-cache measurement;
* `diag-rayon.sh`, which checks 10,000 cached steps with `RAYON_NUM_THREADS` unset, set to 1, and set to 4,
  passed to `kizz` beside `PATH` and `KIZZ_CACHE_DIR`.

Their raw results are in `measure.tsv`, `discovery.tsv`, `cleanup.tsv`, and `diag.tsv`, with one log per run under `logs`.

### Untraced baselines

The user allowed the project's own build commands to run directly, only inside the temporary copies,
and only for the untraced baseline.
These are all the commands run under that permission, each in its own copy:

| Label | Copy | Command |
| --- | --- | --- |
| `trial-ts-baseline` | `$ROOT/trial/ts-base` | `env -i PATH="$PATH" make` |
| `trial-go-baseline-cold` | `$ROOT/trial/go-base` | `env -i PATH="$PATH" HOME="$HOME" GOCACHE=$ROOT/trial/gocache-base GOPROXY=off GOTOOLCHAIN=local go build ./...` |
| `trial-rust-baseline` | `$ROOT/trial2/ts-base` | `env -i PATH="$PATH" HOME="$HOME" cargo build --offline` |
| `ts-make-baseline-N`, N = 1 to 5 | `$ROOT/measure/ts-make-base-N` | `env -i PATH="$PATH" make` |
| `ts-cargo-baseline-N`, N = 1 to 5 | `$ROOT/measure/ts-cargo-base-N` | `env -i PATH="$PATH" HOME="$HOME" cargo build --offline` |
| `go-cold-baseline-N`, N = 1 to 3 | `$ROOT/measure/go-base-N` | `env -i PATH="$PATH" HOME="$HOME" GOCACHE=$ROOT/measure/gocache-base-N GOPROXY=off GOTOOLCHAIN=local go build ./...` |
| `go-warm-baseline-N`, N = 1 to 5 | `$ROOT/measure/go-base-3` | `env -i PATH="$PATH" HOME="$HOME" GOCACHE=$ROOT/measure/gocache-base-3 GOPROXY=off GOTOOLCHAIN=local go build ./...` |

The three trial runs checked that each build completes before the measured runs; their times are not used.
`$ROOT/q2-commands.log` lists the same 21 runs with their start times and full commands.
No other command ran a build tool by hand; the traced builds ran through `kizz build`.

## Tracing overhead

The `inner` time is the build command's own time inside the trace, from the `-timed` tasks,
so the difference from the baseline is what tracing costs the command,
and the difference from the `kizz build` total is what `kizz` adds around it: starting, evaluating the task,
recording the state of every touched path, and the confirming pass.

| Build | Untraced | Traced, inside | Command slowdown | `kizz build` total | Total slowdown |
| --- | --- | --- | --- | --- | --- |
| `true` | - | - | - | 0.017 | - |
| `make`, tree-sitter-cddl | 0.057 | 0.071 | +25% (+0.014) | 0.104 | +82% (+0.047) |
| `cargo build --offline`, tree-sitter-cddl | 0.580 | 0.666 | +15% (+0.086) | 0.734 | +27% (+0.154) |
| `go build ./...`, cold Go build cache, 3 runs | 5.086 | 5.872 | +15% (+0.786) | 6.018 | +18% (+0.932) |
| `go build ./...`, warm Go build cache | 0.081 | 0.162 | +100% (+0.081) | 0.239 | +195% (+0.158) |

* The traced cold Go build used the same user time as the untraced one, 45 seconds, and 6.3 rather than 4.8 seconds of system time:
  the cost is in the stops for tracked system calls, not in the compilers.
* The warm Go build compiles nothing and spends its time checking files, nearly all of them tracked calls,
  which is the tracer's worst case.
* The rest of the `kizz build` run, the `-timed` total less the `inner` time, takes 32 milliseconds for `make`, 72 for `cargo`,
  176 for the cold Go build, and 85 for the warm one, which records 8,485 paths.
  It covers starting, evaluating the task, recording, and the confirming pass; the no-op task alone takes 17.

## Cache checks

A check is a `kizz build` of the same target again, every step a cache hit.

| Build | Recorded paths | Database | Check | Check after a change of metadata only |
| --- | --- | --- | --- | --- |
| `true` | - | - | 0.013 | - |
| `make`, tree-sitter-cddl | 163 | 38 KiB | 0.014 | 0.106, the command runs; then 0.014 |
| `cargo build --offline`, tree-sitter-cddl | 1,069 | 279 KiB | 0.017 | 0.273, the command runs; then 0.019 |
| `go build ./...` | 8,485 | 1,515 KiB | 0.043 | 0.265, the command runs; then 0.045 |
| 50,000 steps of `cat`, one input each | 50,103 | 74 MiB | 9.0 | 10.6, every input hashed again, all cached; then 9.0 |

* Recorded paths are the ones `kizz cache clean` checks: every recorded path that existed, the dependencies a file deletion affects.
* Touching every committed file re-runs `make`, `cargo`, and `go`, as the spec requires:
  each of them only checked the metadata of some inputs, such as `grammar.js` or `.gitignore`,
  and "a regular file that was not read has no hash, so its metadata is the whole dependency".
  Touched files that were read are hashed, found unchanged, and get their new metadata, so the next check is fast again.
* The first Go check after the cold build ran the command again:
  Go's telemetry counter files under `~/.config/go/telemetry` had been changed by the other `go` runs of the measurement.
  Kizz is right to re-run, and a Go task can leave them out with `ignore = ["~/.config/go/telemetry/**"]`;
  the median is over the four later checks.
* The 50,000-step check costs 0.18 milliseconds per step, but 100 seconds of user time on 32 threads.
  Checking 10,000 such steps took 1.53 seconds with the default thread pool, 0.83 seconds with `RAYON_NUM_THREADS=1`,
  and 0.73 seconds with 4 threads, using 19.6, 0.6, and 0.8 seconds of user time:
  the per-step parallel comparison costs more than it saves when a step has a handful of paths.

## Discovery

`kizz list` ran in the root of each tree, and `find ROOT -name kizz.md` ran as a reference for a plain walk.
The generated trees come from `$ROOT/gen-tree.sh`,
which creates four empty files in every leaf directory and one `kizz.md` with one task in every second-level directory.

| Tree | Directories | Files | `kizz.md` files | `kizz list` | `find` |
| --- | --- | --- | --- | --- | --- |
| generated, `gen-tree.sh ROOT 10 10 100` | 10,111 | 40,100 | 100 | 0.018 | 0.088 |
| generated, `gen-tree.sh ROOT 50 50 40` | 102,551 | 402,500 | 2,500 | 0.145 | 0.864 |
| cosmos-sdk copy | 917 | 4,263 | 1 | 0.006 | 0.009 |

The parallel walk is six times faster than `find` on the large tree, and discovery needs no follow-up work.

## Cache cleanup on a large cache

Step 14 asked for this measurement, because `kizz cache clean` and `kizz cache reset` hold every step record in memory.
`$ROOT/cleanup.sh 50 1000 large` built 50 projects of one task each, every task running `cat data/N.txt` for N from 0 to 999,
into one cache of 50,000 step records and 74 MiB.

| Command | Time |
| --- | --- |
| `kizz build fill`, filling the cache: 50,000 traced commands | 164.4 |
| `kizz cache clean`, nothing gone, 50,103 recorded paths, 3 runs | 0.612 |
| `kizz cache clean` after deleting 25 projects: 25,000 step records removed | 0.806 |
| `kizz cache clean` again, 25,053 paths, nothing gone | 0.292 |
| `kizz cache reset --yes` in one project: 1,000 step records | 0.164 |
| `kizz cache reset --all --yes`: 24,000 step records | 0.028 |

Holding the records in memory is not a problem at this size.
Filling took 3.3 milliseconds per traced `cat`, against 2.1 milliseconds per step for 10,000 steps in an empty cache,
which suggests that the cost of a step grows with the size of the cache (follow-up item 2).

## The fast-path decision

Decision: the tracer gets no fast path in version 1.

* While a command runs, tracing costs 15 to 25 percent on builds that compile, which the spec accepts as the price of complete,
  automatic dependencies.
* The worst case measured is a build that only checks metadata: a warm `go build ./...` takes twice as long, 81 milliseconds more.
  Such a build runs only when one of its inputs changed; otherwise kizz replays it in a 43-millisecond check.
* The larger costs found are outside the tracer: per-step thread-pool overhead in checks and recording,
  and the growth of the per-step cost with the cache size.
  A tracer fast path would not reduce them.
* A fast path, such as the preload library Riker added to avoid stops for common calls,
  stays follow-up work behind the tracer boundary.
  It becomes worth building when a real project's command slows by more than about 30 percent under tracing, measured as here,
  after the follow-up items below are done.

## Defect fixed in this phase

A traced `cargo build --offline` was never cached: every following build ran it again, because "`/dev/fd/3` changed
(content changed)" and "`/dev/fd/5`: link target changed".
The command had read and written its inherited descriptors through `/dev/fd`, which is a link to `/proc/self/fd`.
Kizz recorded and checked those paths in its own process, so it compared its own descriptors, which differ on every run,
instead of treating them as the `/proc` accesses they are.

The fix, in `crates/kizz-core/src/observe.rs`, follows the spec's symbolic-link rule, "the link target,
plus the state of what it points to", for links that lead into `/proc` or `/sys`, such as `/dev/fd` and `/dev/stdin`.
Only the final object under `/proc` or `/sys` is untracked and never compared, as the spec says for `/proc` and `/sys`:
`/dev/fd/3` is recorded as untracked, and `/dev/stdin` as a link with its target and an untracked pointee.
Every link on the way stays an ordinary dependency, compared by its target like any link:
a directory link such as `/dev/fd` is recorded as a probed link with its target and an untracked pointee.
Retargeting, removing, or replacing such a link runs the step again.
The regression tests are:

* `observe::tests::kernel_paths_and_the_standard_links_into_them_reach_the_kernel`,
  `observe::tests::links_that_lead_into_proc_or_sys_reach_the_kernel`,
  `observe::tests::other_paths_do_not_reach_the_kernel`, and
  `observe::tests::the_links_that_lead_into_proc_or_sys_are_listed` in `kizz-core`;
* `paths_that_links_lead_into_proc_or_sys_keep_only_their_links` in `crates/kizz-core/tests/engine.rs`;
* `a_final_link_into_proc_stays_a_dependency` and `a_directory_link_into_proc_stays_a_dependency`
  in `crates/kizz-core/tests/cache.rs`, with a fake tracer: a hit while a final link and a directory link into `/proc`
  are unchanged, then a new run for each retarget, removal, and replacement;
* `a_command_that_reads_dev_fd_paths_is_cached_on_the_next_build` and
  `retargeting_or_replacing_a_link_into_proc_runs_the_command_again` in `crates/kizz/tests/build.rs`,
  which run the binary with the real tracer.

With the fix, the second build of the Rust binding is a cache hit,
and `kizz why` lists `/dev/fd/3` and `/dev/fd/5` among the step's untracked accesses.
All the numbers in this report come from the first form of the fix, which recorded such paths and their links as untracked,
except the evidence of the defect itself.
The final form adds one link observation for `/dev/fd`, whose target does not change, so the hits stay hits; its cost,
one more recorded path per step that uses `/dev/fd`, was not measured again.

## Follow-up work

1. Cache checks and recording of small steps:
   the per-step parallel comparison uses 30 times the CPU time of a sequential one and is slower.
   Compare a step's few paths sequentially, or check several steps' paths in one parallel batch,
   and measure again with the 50,000-step cache.
2. Recording cost grows with the cache: 2.1 milliseconds per traced `cat` for 10,000 steps, 3.3 for 50,000.
   Profile the per-step database work, such as the last-writer and step-position updates.
3. Last-writer entries for temporary files: one build leaves entries for paths that were gone when it ended, 30 for `make`,
   666 for `cargo`, and 1,188 for `go build`, which only `kizz cache clean` removes.
   Consider not storing a last-writer entry for a path that is absent when the step is recorded.
4. The spec's "`/proc` and `/sys`" section should say how paths that symbolic links lead into them, such as `/dev/fd/N`,
   are recorded, as this phase implemented: the links stay dependencies, and only the object they reach is untracked.
   The `kizz why` line "untracked /proc and /sys accesses" now also lists such objects, and its wording could say so.
5. User documentation: Go tasks should ignore `~/.config/go/telemetry/**`,
   since every `go` command on the machine changes those counters, and kizz adds no exceptions of its own.
6. Metadata-only changes, such as a `git checkout` or `touch` that keeps the content,
   re-run commands that only checked the metadata of their inputs, as `make` and `go` do.
   That follows the spec; any change to it is a decision for the user.
7. The tracer fast path, under the condition in [The fast-path decision](#the-fast-path-decision).
