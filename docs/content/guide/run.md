+++
title = "Running one task"
weight = 7
+++

This page explains `kizz run`, which runs one named task, with arguments, as a command runner.

## Overview

`kizz run <name>` finds the task named `<name>` and runs its program once, showing each command and its output as it runs.
It uses and updates the same cache as `kizz build`,
so a command whose recorded inputs have not changed is normally replayed from the cache instead of running; `--no-cache`,
`cache=never` and `always` make commands run anyway.

```text
kizz run [--no-cache] [--explain] [--fail-unchecked] [--cache-unknown-syscalls]
         [--output=<file>] [--compact] [--json] <name> [-- <arguments>...]
```

## Naming tasks

A task gets its name from the `name=` option in its info string:

````markdown
## Greet

```starlark name=greet
env.inherit("PATH")
who = task.args[0] if task.args else "world"
run("echo", "hello,", who, expect = [0])
```
````

A name uses only letters, digits, `_`, `.` and `-`, and is unique within its `kizz.md`.
A task can have targets as well as a name, and then `kizz build` runs it too.
A task with a name and no targets runs only under `kizz run`.
The [kizz.md page](@/guide/kizz-md.md) explains task options.

## How the name is found

`kizz run` discovers tasks as `kizz build` does: in the current directory and every directory below it.

* The task of that name whose `kizz.md` is the fewest directory levels below the current directory wins.
* Two tasks of that name at that nearest level are an error that lists them with their paths:

```text
error: 2 tasks named `hello` are 1 directory level below `.`, so kizz cannot choose one: a/kizz.md:3 (A), b/kizz.md:3 (B)
```

* Tasks of that name at deeper levels are no error, because the nearer task wins.
* A name that no task has is an error that lists the task names under the current directory:

```text
error: no task named `nope` under `.`; the task names under `.` are: greet
```

* A configuration error in any `kizz.md` under the current directory fails `kizz run`, as it fails a build.

So the same name can mean a different task in each part of a project, and you pick one by where you run it.

## Arguments and task.args

The words after `--` are the task's arguments, and its program reads them as `task.args`, a list of strings:

```sh
kizz run greet -- kizz
```

* Arguments come only after `--`; a word after the name without `--` is a usage error.
* Arguments must be valid UTF-8, because `task.args` holds strings.
* Under `kizz build`, `task.args` is an empty list.
* The arguments are not part of the task program's hash.
  They reach commands only through what the program does with them, which kizz tracks as usual:
  when the program passes an argument to a command, a new argument runs that command again.
* Under `kizz run`, `task.selected` is an empty list, because no target selected the task.

To pass every argument through to a command, give `task.args` to `run()` as one positional argument:

```starlark
run("cargo", "test", task.args)
```

`run()` expands the list in place, so for a task named `test` with this call,
`kizz run test -- --release foo` runs `cargo test --release foo`, each argument as one word, and `kizz run test` runs `cargo test`,
because an empty `task.args` adds nothing.
A variable that holds `[]` or a list of words works the same way for an argument that only some runs need;
the [`argv` entry](@/reference/starlark.md#argv) of the reference gives every rule.

## Live output and cached replay

`kizz run` shows each command as it starts, then its output as it comes, as if every `run()` had `stream = True`.
A cache hit shows its command marked as cached, then replays its stored output.
With `split` capture, the default, a command's standard output goes to standard output and its standard error to standard error.
With `merged` capture, both of its output streams are one stream, which goes to standard output.
Both hold live and when a cache hit replays the stored output.

```text
run `echo hello, world` (step 1): never run: there is no record for this step
hello, world
run succeeded: kizz.md:13 (Greet); 1 command ran, 0 cached
```

Run it again, and the command comes from the cache:

```text
cached `echo hello, world` (step 1)
hello, world
run succeeded: kizz.md:13 (Greet); 0 commands ran, 1 cached
```

* Each line that starts a command gives the one-line reason it ran; `--explain` prints the full reasons under it instead.
* A `run()` that fails before kizz records its step or asks the tracer to run it shows no line,
  and the summary counts it neither as a command that ran nor as a cached one: a bare program name without `PATH`,
  an invalid `ignore` or `track` pattern, a `~/` pattern without `HOME`, or a failure of kizz's own state,
  such as a cache that it cannot read.
* A program that is not found or not executable, a command that cannot start,
  and a tracer failure show their line and are counted as commands that ran.
* The task's `print` output, its failure and every warning print as in a build, and a one-line summary closes the run.
* The banner comes first, unless `--quiet` or `--no-banner` is given.

## Scope of a run

* The task runs once: no other task runs, and no pass follows, even when its program changes a file.
* `kizz run` takes the cache lock, as a build does, and records what a build of that one task would record.
* Unchecked non-zero exits warn after the run, and `--fail-unchecked` makes them fail the task, as in a build.
* `--cache-unknown-syscalls` caches the results of commands that made a system call the tracer does not know, as in a build.
* Ctrl-C works as in a build: the running command receives the interrupt once, nothing new starts, and its result is not cached.

## No cache and cache=never

`--no-cache` runs every command of the task, as if each had `always = True`, and still records each result:

```text
run `echo hello, world` (step 1): `kizz run --no-cache` was given
```

A task marked `cache=never` runs every command in every run and in every build that reaches it,
and so does a command marked `always`.
No option forces caching: `kizz run` can make a cached command run, but never make such a command cached.

## Reports

`--output=<file>` writes an observation report of the run once it ends, in the format of [`kizz execute`](@/guide/execute.md):

```sh
kizz run greet --output=run-report.txt
```

```text
kizz observation report, version 1, hierarchical
trace: complete
totals: commands 1, cached 1, processes 0, programs 0, paths 0, connections 0
paths by access: read 0, probe 0, list 0, write 0, exec 0, xattr 0

command 1: echo hello, world
  program: /usr/bin/echo
  directory: /tmp/demo/hello
  outcome: exited with 0
  trace: cached, not traced

task program: kizz.md:13 (Greet)
```

* Each command that ran is the root of its own process tree, in the order the commands ran.
* A cache hit is listed as cached, without observations; `--no-cache` runs every command, so the report observes them all.
* The task program's own accesses are an entry of their own: the modules it loaded, and its `fs` and `env.load_file` accesses.
* `--compact` and `--json` shape the report as they do for `kizz execute`, and need `--output`:

```text
error: --compact and --json shape the report that --output writes, so they need --output
```

* A missing directory for `--output` is a usage error, found before the run starts.
* A report that cannot be written is an error; a run that otherwise succeeded then exits with 2.

Without `--output`, `kizz run` writes no report, and shows only each command and its output.

## Standard input

A command's standard input is what its `run()` gives with `stdin`, or `/dev/null`, as in a build.
So a command that reads the terminal interactively does not work under `kizz run`.
Planned: interactive commands in `kizz run`, which [planned features](@/guide/planned.md) describes.

## Exit statuses

* 0 when the task succeeds.
* 1 when it fails.
* 2 for usage and configuration errors, such as an unknown or ambiguous name,
  and when the report cannot be written after a run that succeeded.
  An overflow of kizz's stack, such as one in a task program that nests values too deeply, also exits with 2.
* 130 when interrupted.
