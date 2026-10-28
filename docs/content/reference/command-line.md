+++
title = "Command line"
weight = 1
+++

This page lists every command of `kizz` and every option, with what each does, its default, and its exit statuses.

## Overview

```text
kizz [--color=<auto|always|never>] [--quiet] [--no-banner] [-v] <command> ...
```

These are the commands:

* [`kizz build`](#kizz-build) runs the tasks of one or more targets.
* [`kizz why`](#kizz-why) prints why each command of the most recent build ran or was cached.
* [`kizz list`](#kizz-list) lists the tasks that kizz discovers.
* [`kizz cache clean`](#kizz-cache-clean) and [`kizz cache reset`](#kizz-cache-reset) clean up the cache.
* [`kizz execute`](#kizz-execute) runs any program under the tracer and writes a report of what it touched.
* [`kizz run`](#kizz-run) runs one task by its name.
* [`kizz graph`](#kizz-graph) and [`kizz image`](#kizz-image) are planned: they are parsed, but not implemented yet.

Each section below gives a command's synopsis, each option and positional argument with its default, the command's output,
its exit statuses and examples.
The examples use the `kizz.md` below, in the directory `/tmp/ref/demo`, beside a file `notes.txt` that holds `one two three`.
They pass `--no-banner`, so they show only what the command itself prints, and process IDs are those of one machine.

````markdown
# Hello

## Count the words

```starlark ci name=count
env.inherit("PATH")
count = run("wc", "-w", "notes.txt", expect = [0])
fs.write("out/count.txt", count.stdout)
```

## Greet

```starlark name=greet
env.inherit("PATH")
who = task.args[0] if task.args else "world"
run("echo", "hello,", who, expect = [0])
```
````

Kizz writes the lines of its commands to standard output, and its errors, warnings and questions to standard error.
An error starts with `error:` and a warning with `warning:`, and the warnings are repeated in a summary at the end.
A write that fails, such as one to a closed pipe, is dropped, and kizz goes on with the exit status that its work gives.

Exit statuses that the commands share:

* 0 on success;
* 1 when a task fails, for `kizz build` and `kizz run`;
* 2 for a command-line error, a configuration error in a `kizz.md`, a cache that cannot be used, and the planned commands;
* 130 when Ctrl-C interrupts `kizz build` or `kizz run`.

`kizz execute` exits with the status of the program it runs instead, as [its section](#kizz-execute) says.

## `kizz` and its global options

The global options work with every command.
They may come before the command or among the command's own options and arguments, but not after `--`.

```text
kizz [--color=<auto|always|never>] [--quiet] [--no-banner] [-v] <command> ...
kizz --help
kizz --version
```

A command line that does not parse, such as one with an unknown command or option, prints `Error:` and what is wrong,
and exits with 2.
Without a command, `kizz` prints `Error: expected` and `` `COMMAND ...` ``, and exits with 2.

### `--color`

Whether kizz styles its own output with color and bold text: `--color=auto`, `--color=always` or `--color=never`.
`--color auto`, with a space, works too, and any other value is a command-line error.

* `auto`, the default, leaves the choice to the terminal library that kizz uses, `console`,
  for each of standard output and standard error.
  It styles a stream that is a terminal, unless `NO_COLOR` is set, `TERM` is unset or `dumb`, or `CLICOLOR` is `0`.
  `CLICOLOR_FORCE` set to anything but `0` styles both streams, terminals or not, whatever the other variables say.
  [Environment variables](@/reference/environment.md) describes each variable.
* `always` styles both streams, whatever the variables say and wherever the output goes.
* `never` styles neither stream.

The option changes only how kizz prints its own lines: the output of commands and the lines of task programs are not styled.

### `--quiet`

Leaves out the banner, as `--no-banner` does.
It is off by default.

### `--no-banner`

Leaves out the banner, which every command except `kizz execute` prints to standard output before anything else.
It is off by default, so the banner is printed.
Help, the version and command-line errors print no banner, and `kizz execute` never prints it.
The banner is the name kizz in large letters, a line that says what kizz is, and a line with the version and commit:

```text

 _    _
| | _(_)________
| |/ / |_  /_  /
|   <| |/ / / /
|_|\_\_/___/___|
Traced, cached builds from kizz.md files
v0.0.1 (d5186af) - Copyright (c) 2026, Sakura Industries LLC - AGPLv3 Only

```

### `-v`, `--verbose`

Prints more detail; it is off by default.

* `kizz build` also lists each command that came from the cache, once, the first time the build reaches it.
* `kizz build`, `kizz run` and `kizz why` show the values of environment variables and `stdin` text in their reasons.
  Without `--verbose`, a reason names each variable that changed but not its value, because values can be secrets.
* `kizz why` also shows the recorded machine state of every part, not only of the parts that each step names.

The other commands print the same with or without it.

### `-h`, `--help`

Prints the list of commands and the global options to standard output, and exits with 0; no command runs.
Each command also has its own `-h` and `--help`, such as `kizz build --help`.

### `-V`, `--version`

Prints kizz's version, such as `Version: 0.0.1`, to standard output, and exits with 0; no command runs.
It works only before a command.

## `kizz build`

Runs the tasks of the given targets that kizz discovers in the current directory and below, tracing and caching every command.

```text
kizz build [--explain] [--cache-unknown-syscalls] [--fail-unchecked] [--sequential | --parallel] <targets>
```

1. Without a target, `kizz build` discovers the tasks, prints an error that lists the available targets, and exits with 2.
2. It takes the cache lock.
   While another kizz command holds it, `kizz build` prints `waiting for the kizz cache, which is in use by` and the holder,
   and waits.
3. It opens the cache, discovers every `kizz.md` under the current directory, and selects the tasks of the targets.
4. It runs the selected tasks in passes until a pass runs no command and no task program changes a file,
   as [the kizz.md page](@/guide/kizz-md.md) explains.
   A failed task does not stop the build: every pass tries every selected task.
5. It prints the unchecked exits that stand, how the build ended and a summary, then releases and deletes the lock file.

A configuration error in any `kizz.md` under the current directory stops the build before any task runs.
A target that selects no task is a warning that lists the available targets, and the build succeeds with no task.

### `TARGETS`

One target, or several joined with `+`, such as `ci+check`.
A target name uses only letters, digits, `_`, `.` and `-`, and a name given twice counts once.
The build selects every task tagged with any of the targets, and a task tagged with several of them runs once.
There is no default: without it, `kizz build` exits with 2 and lists the available targets.
An empty part, as in `ci+`, or an invalid name is an error, with status 2.

### `--explain`

Prints the full reasons for every command, the cached ones included, under the line of each command.
Each cached command is listed once, the first time the build reaches it.
By default, kizz prints one line for each command that runs, with a one-line reason, and only counts the cached commands.
[Caching](@/guide/caching.md) lists every reason.

### `--cache-unknown-syscalls`

Caches the results of commands that made a system call that the tracer does not know.
By default such a result is not cached, so the command runs again on the next build.
Either way, kizz warns about each such command, naming each unknown call by its number.
The option is not part of a step's key, so a result cached with it is reused by later builds with or without it.

### `--fail-unchecked`

Fails a task whose program leaves a non-zero exit or a signal unchecked: the command's `run()` has no `expect`,
and the program never read the result's `rc` or `signal` and never encoded the result.
The task fails when its program ends, and kizz prints each unchecked command, how it ended and its captured output.
By default, such an exit does not fail the task, and kizz warns about it once the build settles,
for each unchecked exit in the results that stand.
The option is off by default; [the kizz.md page](@/guide/kizz-md.md) explains how a program checks a result.

### `--sequential`

Runs one task at a time.
It is the default, so giving it changes nothing.
`--sequential` and `--parallel` exclude each other: giving both is a command-line error.

### `--parallel`

Planned: `--parallel` will run independent tasks at the same time.
Today the parser accepts it, but `kizz build --parallel` prints `error: parallel execution is not implemented yet` and the rest of
that message, runs nothing, and exits with 2.
[Planned features](@/guide/planned.md) describes it.

### `-h`, `--help`

Prints the help of `kizz build` and exits with 0.

### Output

* `ran`, then the command in backticks, its task, its step and its one-line reason, for each command that runs.
* `cached`, the command, its task and its step, for each command that came from the cache, the first time the build
  reaches it, with `--verbose` or `--explain`.
* With `--explain`, the full reasons under each of those lines, instead of the one-line reason.
* No line for a `run()` that fails before kizz records its step or asks the tracer to run it: a bare program name
  without `PATH`, an invalid `ignore` or `track` pattern, a `~/` pattern without `HOME`, a failure of kizz's own state,
  such as a cache that it cannot read, or a step again after a tracer failure in an earlier pass, which counted it then;
  the summary counts such a call neither as a command that ran nor as a cached one.
* A program that is not found or not executable, a command that cannot start, such as one whose `cwd` does not exist,
  and a tracer failure get their `ran` line and are counted as commands that ran.
* `pass` and its number at the start of each pass after the first, with what the pass before did: a command ran,
  a task program changed a file, or both.
* What each task program prints with `print`, after its task's name, else its heading, else its `kizz.md` and line.
* For a failed task, on standard error, `error:`, the task, the error, the captured output of the failing command and the
  Starlark stack.
* Loud warnings, on standard error: a command traced incompletely, with the reason;
  a file that something else changed while a command ran, after the command read it;
  a command that made system calls that the tracer does not know;
  a command that connected to the network or to a Unix socket;
  and a step that runs again in consecutive builds for a reason that it causes itself.
* Once the build settles, a warning for each unchecked exit in the results that stand, which names the task, the command,
  how it ended, and whether the result came from the cache.
* When the build failed, `failed:` and each failed task, with the paths that it looked for and did not find.
* A summary line: `build succeeded`, `build failed`, `build did not settle` or `build interrupted`, then the number of tasks,
  of failed tasks when any failed, of passes, and of the commands that ran and that came from the cache.
* At the end, the number of warnings and each warning again, on standard error.

When the build has not settled after one more pass than it has tasks, kizz stops, says what the last pass still did,
and names the inputs that kept changing and who wrote them, the files that task programs kept changing,
or else the tasks that kept running commands.

On Ctrl-C, kizz starts nothing new, and the running command receives the interrupt once.
Kizz kills whatever of the command still runs 2 seconds later, and the command's result is not cached.
Kizz then closes the cache, releases and deletes the lock file, and exits with 130.

### Exit statuses

* 0 when the build succeeds, which a target that selects no task also gives.
* 1 when a task fails in the pass that ends the build, or when the build does not settle.
* 2 without a target, for an invalid target expression, for a configuration error, for `--parallel`,
  and for a cache that cannot be located, locked, opened or used, such as one that belongs to another machine or user.
  Kizz's own errors, such as a thread that it cannot start, also exit with 2.
* 130 when Ctrl-C interrupts the build.

### Examples

A first build runs the command of the task tagged `ci`, then checks the task again in a second pass:

```sh
kizz --no-banner build ci
```

```text
ran `wc -w notes.txt` in kizz.md:5 (Count the words) step 1: never run: there is no record for this step
pass 2: a command ran and a task program changed a file in pass 1, so every selected task is checked again
build succeeded: 1 task, settled in 1 pass; 1 command ran, 0 cached
```

The next build finds nothing changed, and `--verbose` lists the cached command:

```sh
kizz --no-banner --verbose build ci
```

```text
cached `wc -w notes.txt` in kizz.md:5 (Count the words) step 1
build succeeded: 1 task, settled in 1 pass; 0 commands ran, 1 cached
```

After `notes.txt` changes to `one two three four`, `--explain` prints the full reason:

```sh
kizz --no-banner build --explain ci
```

```text
ran `wc -w notes.txt` in kizz.md:5 (Count the words) step 1
  ran because:
    1 input changed:
      `notes.txt`: size changed from 14 to 19 bytes; last written by something outside kizz
pass 2: a command ran and a task program changed a file in pass 1, so every selected task is checked again
build succeeded: 1 task, settled in 1 pass; 1 command ran, 0 cached
```

In another directory, a task whose command exits with a code that its `expect` does not accept fails the build, with status 1.
Its `kizz.md` has the heading "Check the config" on line 3, and on line 7, under `env.inherit("PATH")`,
the call `run("cat", "config.txt", expect = [0])`, while `config.txt` does not exist:

```text
ran `cat config.txt` in kizz.md:5 (Check the config) step 1: never run: there is no record for this step
error: kizz.md:5 (Check the config) failed:
  command `cat config.txt`: exit code 1, but `expect` accepts exit code 0
  --- stdout ---
  --- stderr ---
  cat: config.txt: No such file or directory
  Traceback (most recent call last):
    * kizz.md:7, in <module>
        run("cat", "config.txt", expect = [0])
  error: run(): command `cat config.txt`: exit code 1, which `expect` does not accept
   --> kizz.md:7:1
    |
  7 | run("cat", "config.txt", expect = [0])
    | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
    |
pass 2: a command ran in pass 1, so every selected task is checked again
failed: kizz.md:5 (Check the config)
  it looked for these paths and did not find them:
    `/etc/ld.so.preload`
    `config.txt`
build failed: 1 task, 1 failed, settled in 1 pass; 1 command ran, 0 cached
```

Without a target, and with `--parallel`, the build exits with 2:

```sh
kizz --no-banner build
```

```text
error: no target given; give one target, or several joined with `+`; the available targets are: ci
```

```sh
kizz --no-banner build --parallel ci
```

```text
error: parallel execution is not implemented yet; `kizz build` runs one task at a time with --sequential, the default
```

## `kizz why`

Prints the reasons that the most recent build recorded for each command of the tasks under a path.

```text
kizz why [<path>]
```

It runs no task, and it reads the cache under the cache lock, so it waits while a build runs, and says what it waits for.

### `PATH`

The directory whose tasks to show, relative to the current directory; the default is the current directory.
The tasks of every `kizz.md` in that directory or below it are shown.
A path that no longer exists still shows the tasks recorded under it.

### `-h`, `--help`

Prints the help of `kizz why` and exits with 0.

### Output

* `The most recent build ran`, the number of tasks recorded under the path, and the path.
* Each task with its targets, marked `(failed)` when it failed, or `no commands ran` when it ran none.
* For each step: its command, why it ran or was cached, its untracked `/proc` and `/sys` accesses, and its connections.
* The system calls that the step made and that the tracer does not know, when there are any.
* The recorded machine state of each part that the step names, or of every part with `--verbose`.
* How many accesses the step's `ignore` patterns left out.
* A warning for each step that ran again in consecutive builds for a reason that it causes itself.

When no build recorded a task under the path, it prints `No build has recorded a task under` and the path.

### Exit statuses

* 0 when it printed the reasons, or that there are none.
* 2 for a cache that cannot be located, locked, opened or read.

### Examples

After the builds of [`kizz build`](#kizz-build):

```sh
kizz --no-banner why
```

```text
The most recent build ran 1 task under `.`.

kizz.md:5 (Count the words) [ci]
  step 1: `wc -w notes.txt`
    ran because:
      1 input changed:
        `notes.txt`: size changed from 14 to 19 bytes; last written by something outside kizz
    untracked /proc and /sys accesses: none
    connections: none
    ignored accesses: 0
```

```sh
kizz --no-banner why sub
```

```text
No build has recorded a task under `sub`.
```

## `kizz list`

Lists the tasks that kizz discovers in the current directory and below, with any discovery warnings.

```text
kizz list [<targets>]
```

It reads every `kizz.md` but runs no task, and it does not use the cache, so it runs while a build holds the cache lock.

### `TARGETS`

Lists only the tasks of these targets: one target, or several joined with `+`, as for `kizz build`.
By default, every task is listed, including tasks that have a name and no target.
A target that selects no task is no error: no task is listed.

### `-h`, `--help`

Prints the help of `kizz list` and exits with 0.

### Output

* One line for each task, in discovery order: its `kizz.md` and line, its heading in parentheses when a heading is above it,
  its targets in brackets, and its task options, such as `name=count` or `cache=never`.
* A summary: the number of tasks and of `kizz.md` files, or, with targets, how many of the tasks they select.
* Discovery warnings, such as the one for a block with neither a target nor a name, on standard error.

### Exit statuses

* 0 when the tasks are listed.
* 2 for an invalid target expression and for a configuration error in any `kizz.md`, which kizz prints with every warning,
  and for kizz's own errors, such as a current directory that it cannot read.

### Examples

```sh
kizz --no-banner list
```

```text
kizz.md:5 (Count the words) [ci] name=count
kizz.md:13 (Greet) [] name=greet
2 tasks in 1 kizz.md file.
```

```sh
kizz --no-banner list ci
```

```text
kizz.md:5 (Count the words) [ci] name=count
1 of 2 tasks selected by `ci`.
```

## `kizz cache`

The commands that clean up the shared cache: [`kizz cache clean`](#kizz-cache-clean) and [`kizz cache reset`](#kizz-cache-reset).

```text
kizz cache clean
kizz cache reset ...
```

Both take the cache lock first, so neither runs during a build, and each waits and says what it waits for.
They change cache entries only, never project files.
`kizz cache` without one of them is a command-line error, with status 2.
[Caching](@/guide/caching.md) explains what the cache holds.

### `-h`, `--help`

Prints the two commands and exits with 0.

## `kizz cache clean`

Removes the cache entries about files and dependencies that no longer exist, without asking.

```text
kizz cache clean
```

* It checks every recorded path.
* A step record that depends on a file that existed when it was recorded and no longer exists is removed as a whole.
* Task records, step positions and learned order of `kizz.md` files that no longer exist are removed too.
* So are the last-writer entries of paths that no longer exist.
* A step record that it keeps loses the tasks whose `kizz.md` no longer exists as owners.
* Dependencies on files that did not exist when they were recorded stay, because they are still valid.

It takes no option or argument besides help.

### `-h`, `--help`

Prints the help of `kizz cache clean` and exits with 0.

### Output

The number of recorded paths checked and of those that no longer exist, then what was removed, family by family,
or that nothing was removed.

### Exit statuses

* 0 when the clean finished, whether or not it removed anything.
* 2 for a cache that cannot be located, locked, opened or cleaned.

### Examples

```sh
kizz --no-banner cache clean
```

```text
checked 8 recorded paths; 0 no longer exist
nothing to remove: no cache entry is about a file that no longer exists
```

## `kizz cache reset`

Wipes every cache entry of the tasks under a path, or the whole cache with `--all`,
after it shows what it will remove and asks for confirmation.

```text
kizz cache reset [--yes] [<path>]
kizz cache reset --all [--yes]
```

* A path reset removes the step records, step positions, task records and learned order of the tasks under the path,
  and the last-writer entries of paths under it.
  A step record that tasks outside the path also own is kept, without the tasks under it.
* `--all` wipes the whole cache, also one of another schema version or one that kizz refuses to use because of its identity.
* Each reset first prints what it will remove: the number of tasks, and of the entries of each family.
* It then asks `Remove these entries? [y/N]` on standard error and reads the answer from standard input.
  `y` or `yes`, in any case, goes ahead; any other answer removes nothing.
* Unless standard input and standard error are both terminals, a reset refuses to run without `--yes`.
* When there is nothing to remove, it says so and asks nothing.

### `PATH`

The directory whose tasks' entries to wipe, relative to the current directory; the default is the current directory.
A path that no longer exists still matches the entries recorded under it.
A path with `--all` is an error, with status 2.

### `--all`

Wipes the whole cache instead of the entries under a path.
Before asking, it says if the cache has another schema version, belongs to another machine or user, or has no cache identity.
It is off by default.

### `--yes`

Skips the confirmation, so the reset goes ahead at once, also without a terminal.
It is off by default.

### `-h`, `--help`

Prints the help of `kizz cache reset` and exits with 0.

### Output

* First, `kizz cache reset wipes every entry for the tasks under` and the path, or with `--all`,
  `kizz cache reset --all wipes the whole cache at` and the cache directory, then the tasks and entries.
* Then `removed` and the entries of each family, or with `--all`, `wiped the whole cache: removed` and the entries.
* When the answer is not yes, `reset declined; nothing was removed` instead.

### Exit statuses

* 0 when the reset finished, also when the answer declined it or there was nothing to remove.
* 2 for a path with `--all`, for a missing terminal without `--yes`, for an answer that cannot be read, and for a cache
  that cannot be located, locked, opened or changed.

### Examples

After the runs of [`kizz run`](#kizz-run),
`kizz cache reset --yes` printed a line
that begins `` kizz cache reset wipes every entry for the tasks under `/tmp/ref/demo`: 2 tasks; 3 step records, ``,
then the line below; the next reset found nothing to remove:

```text
removed 3 step records, 2 step positions, 2 task records, 0 learned-order entries, 1 last-writer entry
```

```text
the cache has no entries for tasks under `/tmp/ref/demo`; nothing to remove
```

Without a terminal and without `--yes`, a reset prints what it would remove, then refuses, with status 2:

```text
error: refusing to reset the cache without a terminal to ask for confirmation; pass --yes to reset without asking
```

## `kizz execute`

Runs one program, traces its whole process tree, and writes an observation report of every path that it touched.

```text
kizz execute --output=<file> [--compact] [--json] -- <program> [<arguments>...]
```

It needs no `kizz.md` and no cache: it never opens the cache, never takes its lock, and never creates a cache directory.
It never caches anything, so every run executes the program.
[Observing a command](@/guide/execute.md) explains what it is for and how to read its reports.

* The program and its arguments come after the required `--`, and the program runs without a shell.
* A program name with `/` names a file, relative to the current directory unless it is absolute.
  A name without `/` is looked up in kizz's own `PATH`, as `run()` looks one up in a task's.
  Each location that the lookup checks is an observation of the command's first program in the report.
  Without `PATH` in kizz's environment, a name without `/` is not found.
* The program gets kizz's whole environment, unchanged, and kizz's current directory.
* It keeps kizz's standard input, output and error, its process group and its session, so it keeps the terminal.
  Kizz prints no banner, writes nothing to standard output, and writes to standard error only its own errors,
  such as a program that could not start, a tracer that failed, or a report that could not be written.
* Kizz waits for the whole process tree to end, then writes the report to a new hidden file in the directory of `<file>`
  and renames it over `<file>`, so the program never sees the report.
* The report is written whatever happened: the program exited, was killed or could not start, or tracing failed.
  If kizz itself is killed, the traced processes die with it, and no report is written.

Signals, while the program runs:

* Kizz ignores `SIGINT` and `SIGQUIT`, as `time` does, and the program gets their default action, so Ctrl-C reaches it.
* Kizz forwards a `SIGTERM` or `SIGHUP` that it receives to the program; one sent to the whole process group arrives twice.
* Of these four signals, one that was ignored when kizz started stays ignored, for kizz and for the program.
* Ctrl-Z stops kizz with the program, and `fg` continues both.
  A traced process may still run after Ctrl-Z until its next traced system call or signal.

### `--output`

The file that the report replaces: `--output=<file>` or `--output <file>`.
It is required; there is no default.
A relative path is relative to the current directory.
Before the program starts, kizz checks that the path names a file, not a directory, and that the directory that holds it exists;
either failure is an error, with status 2, and nothing runs.

### `--compact`

Writes a compact report, which combines the observations of every process into one set and lists no process.
By default the report is hierarchical: each process, with its programs and the paths that each program touched.

### `--json`

Writes the report as JSON instead of text.
By default the report is text.
[The kizz execute report](@/reference/execute-report.md) describes both formats and every JSON field.

### `PROGRAM`

The program to run and its arguments, after `--`.
Words after `--` that look like options, such as `-j8`, are arguments of the program.
The program is required: a missing `--`, or nothing after it, is a command-line error, with status 2.
The arguments need not be valid UTF-8.

### `-h`, `--help`

Prints the help of `kizz execute` and exits with 0.

### Exit statuses

* The program's exit code, or 128 plus the number of the signal that killed it, such as 143 for `SIGTERM`.
* 127 when the program is not found.
* 126 when it is found but cannot be run or started.
* 125 when kizz itself fails after it accepted the command line: the tracer failed, or the report could not be written.
* 2 for a command-line error, such as a missing `--output` or a missing directory for it; then nothing runs.

### Examples

```sh
PATH=/usr/bin kizz execute --output=report.txt -- wc -w notes.txt
```

`wc` prints `4 notes.txt` to the terminal as usual, kizz exits with 0, and `report.txt` holds the report:

```text
kizz observation report, version 1, hierarchical
trace: complete
totals: commands 1, cached 0, processes 1, programs 1, paths 6, connections 0
paths by access: read 3, probe 2, list 0, write 0, exec 2, xattr 0

command 1: wc -w notes.txt
  program: /usr/bin/wc
  directory: /tmp/ref/demo
  outcome: exited with 0
  trace: complete
  process 1, pid 636204, exited with 0
    program /usr/bin/wc: wc -w notes.txt
      read:
        /etc/ld.so.cache: ok
        /lib/x86_64-linux-gnu/libc.so.6: ok
        /tmp/ref/demo/notes.txt: ok
      probe:
        /etc/ld.so.preload: ENOENT
        /usr/bin/wc: ok
      exec:
        /lib64/ld-linux-x86-64.so.2: ok
        /usr/bin/wc: ok
```

A program that is not found gives status 127, this error, and a report:

```sh
PATH=/usr/bin kizz execute --output=report.txt -- no-such-tool
```

```text
error: program `no-such-tool` was not found in PATH
```

Command-line errors give status 2 and run nothing:

```sh
kizz execute --output=missing/report.txt -- true
```

```text
error: the directory of --output does not exist: /tmp/ref/demo/missing
```

```sh
kizz execute --output=report.txt true
```

```text
Error: expected `PROGRAM` to be on the right side of `--`
```

## `kizz run`

Runs one task by its name, once, showing each command and its output as it runs.

```text
kizz run [--no-cache] [--explain] [--fail-unchecked] [--cache-unknown-syscalls]
         [--output=<file>] [--compact] [--json] <name> [-- <arguments>...]
```

1. It checks `--output`, `--compact` and `--json`, then takes the cache lock, waiting as `kizz build` does.
2. It opens the cache and discovers every `kizz.md` under the current directory;
   a configuration error in any of them fails the run with 2.
3. It selects the task named `<name>` whose `kizz.md` is the fewest directory levels below the current directory.
   Two or more such tasks at that level are an error that lists each with its path,
   and a name that no task has is an error that lists the task names under the current directory.
4. It runs the task's program once, with the cache, as a build runs it, and with its arguments in `task.args`.
   No other task runs, and no pass follows, even when the program changes a file.
   Under `kizz run`, `task.selected` is an empty list.
5. It prints the unchecked exits, how the run ended and a summary, writes the report when `--output` is given,
   and releases and deletes the lock file.

A command's `stdin` is what its `run()` gives, or `/dev/null`, as in a build.
Planned: interactive commands, which would read the terminal; [planned features](@/guide/planned.md) describes them.

### `NAME`

The name of the task, which the `name=` option of its info string gives, such as `greet` for `starlark name=greet`.
It is required.
[Running one task](@/guide/run.md) explains how the name is found.

### `ARGUMENTS`

The task's arguments, after `--`, which its program reads as `task.args`, a list of strings.
By default there are none, and `task.args` is an empty list.
Words after `--` that look like options, such as `--fix`, are arguments of the task.
A word after the name without `--` is a command-line error, and so is an argument that is not valid UTF-8; both exit with 2.
The arguments are not part of the task program's hash: they reach commands only through what the program does with them.
A program passes them all to a command with `run("cargo", "test", task.args)`, because `run()` expands a list in place:
each argument stays one word, and an empty `task.args` adds nothing.

### `--no-cache`

Runs every command of the task, as if each had `always = True`, and still records each result.
By default a command whose recorded inputs did not change is replayed from the cache.
No option forces caching: a command marked `always`, or of a task marked `cache=never`, runs whether or not it is given.

### `--explain`

Prints the full reasons under the line of each command, instead of the one-line reason, as for `kizz build`.
It is off by default.

### `--fail-unchecked`

Fails the task when its program leaves a non-zero exit or a signal unchecked, as for `kizz build`.
By default such an exit is a warning after the run.

### `--cache-unknown-syscalls`

Caches the results of commands that made a system call that the tracer does not know, as for `kizz build`.
By default such a result is not cached.

### `--output`

Writes an observation report of the run's commands to this file once the run ends: `--output=<file>` or `--output <file>`.
By default no report is written.

* The report has the format of `kizz execute`, which [the report reference](@/reference/execute-report.md) describes.
* Each command that ran is the root of its own process tree, in the order the commands ran.
* A cache hit is listed as cached, without observations, so `--no-cache` observes every command.
* The task program's own accesses are an entry of their own, such as the files that it writes with `fs.write`.
* A relative path is relative to the current directory.
  A path that names a directory, or a file in a directory that does not exist, is an error, with status 2,
  found before the cache lock is taken, so nothing waits and nothing runs.
* The report is written whether the run succeeded, failed or was interrupted, through a new hidden file renamed over `<file>`.
  It is not written when kizz could not run the task at all, such as for an unknown name.
* A report that cannot be written is an error; a run that otherwise succeeded then exits with 2, and any other status stays.

Only with `--output` does kizz ask the tracer for each command's process tree.

### `--compact`

Writes a compact report, which combines the observations of every process into one set.
It needs `--output`, and it is off by default, so the report is hierarchical.

### `--json`

Writes the report as JSON instead of text.
It needs `--output`, and it is off by default, so the report is text.
`--compact` or `--json` without `--output` is an error, with status 2.

### `-h`, `--help`

Prints the help of `kizz run` and exits with 0.

### Output

* `run`, the command in backticks, its step and its one-line reason, as each command starts, then its output as it comes,
  as if every `run()` had `stream = True`.
* `cached`, the command and its step, for each cache hit, then its stored output.
* No line for a `run()` that fails before kizz records its step or asks the tracer to run it: a bare program name
  without `PATH`, an invalid `ignore` or `track` pattern, a `~/` pattern without `HOME`, or a failure of kizz's own state,
  such as a cache that it cannot read; the summary counts such a call neither as a command that ran nor as a cached one.
* A program that is not found or not executable, a command that cannot start, such as one whose `cwd` does not exist,
  and a tracer failure get their `run` line and are counted as commands that ran.
* With `split` capture, the default, a command's standard output goes to standard output and its standard error to
  standard error; with `merged` capture, its one stream goes to standard output.
* What the task program prints, its failure and every warning, as in a build, and the warnings about unchecked exits after the run.
* A summary line: `run succeeded`, `run failed` or `run interrupted`, the task, and the number of commands that ran and that
  came from the cache.

On Ctrl-C, the run stops as a build does: nothing new starts, the running command receives the interrupt once,
its result is not cached, and kizz exits with 130.

### Exit statuses

* 0 when the task succeeds.
* 1 when it fails.
* 2 for a command-line or configuration error, an unknown or ambiguous name, a cache that cannot be used,
  and a report that cannot be written after a run that succeeded.
* 130 when Ctrl-C interrupts the run.

### Examples

```sh
kizz --no-banner run greet
```

```text
run `echo hello, world` (step 1): never run: there is no record for this step
hello, world
run succeeded: kizz.md:13 (Greet); 1 command ran, 0 cached
```

Run again, the command comes from the cache, and its stored output is replayed:

```text
cached `echo hello, world` (step 1)
hello, world
run succeeded: kizz.md:13 (Greet); 0 commands ran, 1 cached
```

With an argument and `--explain`, then with `--no-cache`:

```sh
kizz --no-banner run --explain greet -- kizz
```

```text
run `echo hello, kizz` (step 1)
  ran because:
    the arguments changed:
      old: echo hello, world
      new: echo hello, kizz
      word 3: `world` became `kizz`
hello, kizz
run succeeded: kizz.md:13 (Greet); 1 command ran, 0 cached
```

```sh
kizz --no-banner run --no-cache greet -- kizz
```

```text
run `echo hello, kizz` (step 1): `kizz run --no-cache` was given
hello, kizz
run succeeded: kizz.md:13 (Greet); 1 command ran, 0 cached
```

A report of the task named `count`, whose program writes a file with `fs.write`:

```sh
PATH=/usr/bin kizz --no-banner run --no-cache --output=run.txt count
```

```text
run `wc -w notes.txt` (step 1): `kizz run --no-cache` was given
4 notes.txt
run succeeded: kizz.md:5 (Count the words); 1 command ran, 0 cached
```

`run.txt` then holds:

```text
kizz observation report, version 1, hierarchical
trace: complete
totals: commands 1, cached 0, processes 1, programs 1, paths 7, connections 0
paths by access: read 3, probe 2, list 0, write 1, exec 2, xattr 0

command 1: wc -w notes.txt
  program: /usr/bin/wc
  directory: /tmp/ref/demo
  outcome: exited with 0
  trace: complete
  process 1, pid 635138, exited with 0
    program /usr/bin/wc: wc -w notes.txt
      read:
        /etc/ld.so.cache: ok
        /lib/x86_64-linux-gnu/libc.so.6: ok
        /tmp/ref/demo/notes.txt: ok
      probe:
        /etc/ld.so.preload: ENOENT
        /usr/bin/wc: ok
      exec:
        /lib64/ld-linux-x86-64.so.2: ok
        /usr/bin/wc: ok

task program: kizz.md:5 (Count the words)
  write:
    /tmp/ref/demo/out/count.txt: ok
```

Errors that exit with 2:

```sh
kizz --no-banner run nope
```

```text
error: no task named `nope` under `.`; the task names under `.` are: count, greet
```

```sh
kizz --no-banner run greet extra
```

```text
Error: expected `ARGUMENTS` to be on the right side of `--`
```

```sh
kizz --no-banner run --json greet
```

```text
error: --compact and --json shape the report that --output writes, so they need --output
```

## `kizz graph`

Planned: `kizz graph` will show the learned task graph, the order in which tasks consume what other tasks write.
Today the command is parsed but not implemented: it prints that it is not wired to behavior yet and exits with 2.
It takes no option or argument besides help.
[Planned features](@/guide/planned.md) describes it.

```sh
kizz --no-banner graph
```

```text
command scaffolded: graph
Show the learned task graph.
target: <none>
This command is parsed but not wired to behavior yet.
```

### `-h`, `--help`

Prints the help of `kizz graph` and exits with 0.

### Exit statuses

* 2, until the command is implemented.

## `kizz image`

Planned: `kizz image` will build container images, reusing each layer while what its steps read has not changed.
Today the command is parsed but not implemented: it prints that it is not wired to behavior yet and exits with 2.
It takes no option or argument besides help.
[Planned features](@/guide/planned.md) describes it.

```sh
kizz --no-banner image
```

```text
command scaffolded: image
Build container images.
target: <none>
This command is parsed but not wired to behavior yet.
```

### `-h`, `--help`

Prints the help of `kizz image` and exits with 0.

### Exit statuses

* 2, until the command is implemented.
