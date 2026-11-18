+++
title = "What kizz is"
weight = 1
+++

This page explains what kizz does, the principles it follows, what it deliberately is not, and the tools it learned from.

## What kizz does

Kizz is a build engine for Linux.
Its work is described in `kizz.md` files: ordinary Markdown files whose fenced `starlark` blocks are tasks.
Each task is a small program in [Starlark][starlark-spec].
A task program runs commands, inspects their results, reads and writes files, and decides what happens next.

When kizz runs a command for a task, it traces the command and its whole process tree.
It records every path that the command read, checked, listed, wrote or ran, including lookups that found nothing,
apart from the paths that the task leaves out with `ignore` patterns.
Those observations are the command's dependencies, and kizz stores them in a cache with the command's exit status and output.
On the next build, kizz checks each recorded path against the file system.
When nothing has changed, it normally skips the command and gives the task program the recorded result instead.
Some commands run again anyway, such as one marked to run on every build or one whose last result could not be cached;
[caching](@/guide/caching.md) lists every case.

These are the commands of `kizz`:

* `kizz build <targets>` runs every task tagged with the targets, in the current directory and below.
* `kizz run <name>` runs one task by its name, once, showing each command and its output live.
* `kizz execute` runs any command under the tracer and reports what it touched, without a `kizz.md` and without the cache.
* `kizz why` explains why each command of the most recent build ran or was cached, and `kizz list` lists the tasks.
* `kizz cache clean` and `kizz cache reset` clean up the shared cache.

Planned: `kizz graph`, to show the learned task graph, and `kizz image`, to build container images.
Today both are parsed but not implemented, as [planned features](@/guide/planned.md) explains.

## Philosophy

Kizz follows a few principles, which the [getting started](@/guide/getting-started.md) page and the rest of the guide put to work.

* Builds should be fast, cacheable and reliable without hand-written dependency lists, which nobody can maintain accurately.
  Nobody declares dependencies; kizz observes them.
* Anything read during the build is an input.
  Kizz is oblivious to why something was read: a configuration file that a tool looks for and does not find is a dependency too.
* Kizz has no knowledge of tools.
  A tool that makes itself uncacheable is reported, not special-cased.
* The user drives exceptions, never dependencies.
  Kizz never leaves a path out on its own; a task leaves out paths with `ignore` patterns.
* Every decision is explainable: kizz says what changed and why it ran a command.
* Nothing depends on Git or on a repository.
  A build starts in the current directory and uses every `kizz.md` below it.
* Behavior kizz cannot observe, such as time, randomness and the network, is assumed absent.
  The user marks the commands that break that assumption, and kizz warns about the network connections it sees.
* A non-zero exit code never fails anything by itself.
  The task program decides which exit codes matter, and kizz warns loudly about the ones that it leaves unchecked.
* Each task starts with an empty environment, and the task chooses every variable that its commands see.

## What kizz is not

Kizz is deliberately narrow.
It is not:

* Hermetic.
  Commands see the real file system, and kizz records what they use instead of limiting what they may use.
* A sandbox.
  Task programs read and write files anywhere and run any command, and a path such as `../../common.txt` is never a violation.
  Run kizz in a container when you want isolation.
* A dependency manager.
  It fetches nothing and resolves no versions; the only inputs that you manage are environment variables.
* A remote cache.
  The cache belongs to one machine and one user, and a shared remote cache is not wanted.
* A host for build daemons, such as the Gradle or Kotlin daemons or an `sccache` server.
  Their work happens outside the traced process tree, so kizz cannot track it; builds that need them should use another tool.
* A tool with knowledge of other tools.
  It tracks a tool's caches at file level like any other files, and adds no exceptions for any tool.
* A tool for any system but Linux on x86_64.
  Other operating systems are not a goal.
  Planned: an aarch64 tracer; today kizz traces only on x86_64, as [planned features](@/guide/planned.md) says.

`kizz execute` is a diagnostic tool, not a replacement for `strace`: it reports paths and how they were used, not every call.

## Prior art

Kizz belongs to a known family of tracing build systems.
These are the closest:

* [Riker](https://www.usenix.org/conference/atc22/presentation/curtsinger) traces plain build scripts with `ptrace` and seccomp-BPF,
  as kizz does; its tracer design and system-call coverage are the reference for kizz's tracer.
* [Rattle](https://arxiv.org/abs/2007.12737) runs traced commands speculatively and detects ordering hazards.
  It informs kizz's learned order and its passes.
* [BuildXL's two-phase cache lookup][buildxl] matches kizz's cache check: a key built from what is known before running,
  followed by the recorded inputs.

[buildxl]: https://github.com/microsoft/BuildXL/blob/master/Documentation/Wiki/Advanced-Features/Two-Phase-Cache-Lookup.md
[starlark-spec]: https://github.com/bazelbuild/starlark/blob/master/spec.md "The Starlark language specification"
