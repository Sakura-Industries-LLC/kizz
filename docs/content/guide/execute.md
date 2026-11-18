+++
title = "Observing a command"
weight = 6
+++

This page explains `kizz execute`, which runs any command under the tracer and reports what it touched.

## Overview

`kizz execute` runs one program, traces its whole process tree, and writes an observation report to a file.
It needs no `kizz.md`, no repository and no cache, and it never caches anything, so every run executes the program.
The report says, for each path the program and its children used, how they used it and with what outcome.
By default it is hierarchical, and lists each process with the programs it ran and the paths each program used; with `--compact`,
it combines every process's observations into one set and lists no process.
It is text unless `--json` is given.

```sh
kizz execute --output=report.txt -- make
kizz execute --compact --json --output=deps.json -- cargo build
```

## What it is for

* Diagnosing existing build systems.
  Trace Make, Cargo, Go or any other tool to see which files, programs and libraries its commands actually use,
  and investigate missing dependencies, unexplained rebuilds, toolchain changes and hidden configuration inputs.
* Investigating unexpected file-system access.
  See what an installation script, a compiler, a development tool or any program read, wrote or looked for.
  This is a diagnostic tool, not a sandbox: it never restricts which paths the program may use.
* Debugging with an AI agent.
  An agent can run a command under `kizz execute`, read the JSON report,
  and reason about its dependencies without interpreting raw `strace` output.
* Evaluating kizz before moving a build to it.
  The report comes from the tracer that builds use, with the kinds of access that builds record,
  and it needs no change to any existing build configuration.
  A build's record of the same command can differ, because a build gives the command its task's environment,
  looks its program up in the task's `PATH`, gives it its `stdin` text or `/dev/null` and pipes as its standard streams,
  and leaves out the paths that the step's `ignore` patterns match.
  You can keep using `kizz execute` alone, or adopt `kizz.md` step by step.

## Running a command

```text
kizz execute --output=<file> [--compact] [--json] -- <program> [arguments...]
```

* `--output` is required, and its directory must exist before the program starts.
  A relative path is relative to the current directory.
* Everything after the required `--` is the program and its arguments, and the program runs directly, without a shell.
  Use `sh -c '...'` for pipelines, redirects and other shell features.
* A program name without `/` is looked up in your `PATH`, as `run()` looks one up in a task's.
  Each location that the lookup checks appears in the report, as an observation of the command's first program,
  because it is a dependency in a build.
  Without `PATH` in your environment, a name without `/` is not found.
* The program gets your whole environment, unchanged, and your current directory, unlike a task, which starts empty.
* It keeps your terminal: its standard input, output and error are kizz's own, and kizz mixes nothing into them.
  Kizz prints no banner, writes nothing to standard output, and writes to standard error only its own fatal errors.
* Kizz waits for the whole process tree to end, then replaces `<file>` with the report in one step.
  The program never sees the report.
* The report is written whatever happened: the program exited, failed, was killed or could not start, or tracing failed.
  If kizz itself is killed, the traced processes die with it, and no report is written.
* Kizz never opens the cache, never takes its lock, and never creates a cache directory.

## Reading a hierarchical report

This report comes from a pipeline that `sh` runs, with `PATH` set to `/usr/bin` to keep it short:

```sh
PATH=/usr/bin kizz execute --output=report.txt -- sh -c 'cat notes.txt | wc -w'
```

```text
kizz observation report, version 1, hierarchical
trace: complete
totals: commands 1, cached 0, processes 3, programs 3, paths 9, connections 0
paths by access: read 3, probe 5, list 0, write 0, exec 4, xattr 0

command 1: sh -c 'cat notes.txt | wc -w'
  program: /usr/bin/sh
  directory: /tmp/demo/hello
  outcome: exited with 0
  trace: complete
  process 1, pid 392913, exited with 0
    program /usr/bin/sh: sh -c 'cat notes.txt | wc -w'
      read:
        /etc/ld.so.cache: ok
        /lib/x86_64-linux-gnu/libc.so.6: ok
      probe:
        /etc/ld.so.preload: ENOENT
        /tmp/demo/hello: ok
        /usr/bin/cat: ok
        /usr/bin/sh: ok
        /usr/bin/wc: ok
      exec:
        /lib64/ld-linux-x86-64.so.2: ok
        /usr/bin/sh: ok
    process 2, pid 392915, child of process 1, exited with 0
      program /usr/bin/cat: cat notes.txt
        read:
          /etc/ld.so.cache: ok
          /lib/x86_64-linux-gnu/libc.so.6: ok
          /tmp/demo/hello/notes.txt: ok
        probe:
          /etc/ld.so.preload: ENOENT
        exec:
          /lib64/ld-linux-x86-64.so.2: ok
          /usr/bin/cat: ok
    process 3, pid 392916, child of process 1, exited with 0
      program /usr/bin/wc: wc -w
        read:
          /etc/ld.so.cache: ok
          /lib/x86_64-linux-gnu/libc.so.6: ok
        probe:
          /etc/ld.so.preload: ENOENT
        exec:
          /lib64/ld-linux-x86-64.so.2: ok
          /usr/bin/wc: ok
```

* The header says whether the trace is complete, then the totals: the commands, the cached commands, the processes,
  the programs that are not inherited, the unique paths, the connections, and the paths of each access kind.
* Each command gives its arguments, its program, its working directory, its outcome, and whether it was traced completely.
  An incomplete trace lists each reason under its `trace:` line.
* Each process gives its number, its process ID, the process that created it, and how it ended.
  Processes are numbered from 1 in the order they started, and process 1 is the command.
  Each sits under the process that created it, and the threads of a process are part of it.
* Each process lists the programs it ran, in order: one for each successful `exec`, with its path and arguments.
  A process that a fork created first runs its creator's program, listed as `inherited` when something belongs to it.
* Under each program, its paths are grouped by access kind: `read`, `probe`, `list`, `write`, `exec`, `xattr`,
  and its `connections`.
  A path appears once in each group, followed by its outcomes.
* An outcome is `ok`, the name of the error that a failed call returned, such as `ENOENT`,
  or `unknown` when the call never returned; an error number without a name reads as `errno` and the number.

Here `sh` looked up `cat` and `wc` in `PATH`, and kizz itself looked up `sh` before starting it,
which is attributed to `sh`'s program.
Each program's loader is an `exec` of the program that it started, and each program looked for `/etc/ld.so.preload`,
which does not exist.
A successful `exec` belongs to the program that it starts, and a failed one to the program that made the call.

Every observation is reported, `/proc` and `/sys` included; the ignore patterns and other rules of builds do not apply.
A text report escapes each backslash and control character of its strings, such as a newline in a path, which reads `\n`.
Paths and arguments that are not valid UTF-8 appear with each invalid sequence replaced by U+FFFD.

## Reading a compact report

`--compact` combines the observations of every process into one set, after the commands, with each path once:

```sh
PATH=/usr/bin kizz execute --compact --output=compact.txt -- sh -c 'cat notes.txt | wc -w'
```

```text
kizz observation report, version 1, compact
trace: complete
totals: commands 1, cached 0, processes 3, programs 3, paths 9, connections 0
paths by access: read 3, probe 5, list 0, write 0, exec 4, xattr 0

command 1: sh -c 'cat notes.txt | wc -w'
  program: /usr/bin/sh
  directory: /tmp/demo/hello
  outcome: exited with 0
  trace: complete

read:
  /etc/ld.so.cache: ok
  /lib/x86_64-linux-gnu/libc.so.6: ok
  /tmp/demo/hello/notes.txt: ok
probe:
  /etc/ld.so.preload: ENOENT
  /tmp/demo/hello: ok
  /usr/bin/cat: ok
  /usr/bin/sh: ok
  /usr/bin/wc: ok
exec:
  /lib64/ld-linux-x86-64.so.2: ok
  /usr/bin/cat: ok
  /usr/bin/sh: ok
  /usr/bin/wc: ok
```

A compact report lists no process, and its observations are exactly the combined observations of the hierarchical report.
It suits dependency inventories and other tools that need to know only what the whole command touched.

## Reading a JSON report

`--json` writes the same report as JSON, for scripts, analysis tools and AI agents.
It names its format, `kizz-execute`, and its version, 1.
A hierarchical report has each command's `processes`, and a compact one has the top-level `observations` and `connections`.

```sh
PATH=/usr/bin kizz execute --compact --json --output=report.json -- wc -w notes.txt
```

```json
{
  "format": "kizz-execute",
  "version": 1,
  "mode": "compact",
  "complete": true,
  "totals": {
    "commands": 1,
    "cached": 0,
    "processes": 1,
    "programs": 1,
    "paths": 6,
    "connections": 0,
    "kinds": {
      "read": 3,
      "probe": 2,
      "list": 0,
      "write": 0,
      "exec": 2,
      "xattr": 0
    }
  },
  "commands": [
    {
      "argv": [
        "wc",
        "-w",
        "notes.txt"
      ],
      "program": "/usr/bin/wc",
      "cwd": "/tmp/demo/hello",
      "cached": false,
      "outcome": {
        "kind": "exited",
        "code": 0
      },
      "complete": true,
      "incomplete": []
    }
  ],
  "task_program": null,
  "observations": [
    {
      "path": "/etc/ld.so.cache",
      "access": {
        "read": [
          "ok"
        ]
      }
    },
    {
      "path": "/etc/ld.so.preload",
      "access": {
        "probe": [
          "ENOENT"
        ]
      }
    },
    {
      "path": "/lib/x86_64-linux-gnu/libc.so.6",
      "access": {
        "read": [
          "ok"
        ]
      }
    },
    {
      "path": "/lib64/ld-linux-x86-64.so.2",
      "access": {
        "exec": [
          "ok"
        ]
      }
    },
    {
      "path": "/tmp/demo/hello/notes.txt",
      "access": {
        "read": [
          "ok"
        ]
      }
    },
    {
      "path": "/usr/bin/wc",
      "access": {
        "probe": [
          "ok"
        ],
        "exec": [
          "ok"
        ]
      }
    }
  ],
  "connections": []
}
```

A new field may appear without a new version, but a field that changes its meaning or disappears raises the version.
The [report reference](@/reference/execute-report.md) describes every field.

## Outcomes and completeness

A command's outcome is one of these:

* `exited`, with its exit code: `outcome: exited with 0`;
* `signaled`, with the number of the signal that killed it: `outcome: signaled with 15`;
* `not_found`, when the program was not found, and `not_executable`, when it was found but cannot be run;
* `start_failed`, when the program was found but could not be started for another reason;
* `failed`, when kizz or its tracer failed; the report then says that the trace is incomplete.

When kizz started no process, the command's tree holds one process without a process ID,
and its program is the name that kizz looked up, with every location the lookup checked:

```text
command 1: no-such-program
  program: not found
  directory: /tmp/demo/hello
  outcome: not found: program `no-such-program` was not found in PATH
  trace: complete
  process 1, not started
    program no-such-program: no-such-program
      probe:
        /usr/bin/no-such-program: ENOENT
```

A report says whether each command was traced completely, and lists every reason that one was not:
a system call through a non-native interface, a call that kizz cannot follow, such as a mount,
a system call that the tracer does not know, a tracer failure, or another reason the tracer gives.
A report never claims a complete trace when the tracer said otherwise.

## What kizz changes for the program

Kizz never restricts which paths the program may use, but it starts every command that it traces, in builds and `kizz run` as well,
with these changes:

* The command runs with the `no_new_privs` flag of Linux, so a set-user-ID program such as `sudo` does not gain privileges,
  as [getting started](@/guide/getting-started.md) says.
* `io_uring_setup` fails with `ENOSYS`, because `io_uring` operations would bypass the tracer's filter,
  so a program that can work without `io_uring` falls back to ordinary system calls, which kizz sees.
* The command starts with an empty signal mask and with the default action for `SIGPIPE`,
  even when kizz was started with signals blocked or with `SIGPIPE` ignored.

## Terminals, signals and job control

The program keeps your terminal, and its signals and job control differ from a run without kizz in these ways:

* While the program runs, kizz ignores `SIGINT` and `SIGQUIT`, as `time` does, and the program gets their default action.
  So Ctrl-C reaches the program, and kizz waits for the tree to end and writes the report.
* Kizz forwards a `SIGTERM` or `SIGHUP` that it receives to the program.
  Kizz cannot tell such a signal from one sent to its whole process group, so one sent to the group arrives twice.
* A signal other than `SIGPIPE` that was already ignored when kizz started stays ignored, for kizz and for the program.
* Ctrl-Z stops kizz with the program, and `fg` continues both.
  A traced process may still run after Ctrl-Z until its next traced system call or signal, where it waits for kizz.
  A stop signal sent to a traced process alone, such as `kill -STOP`, does not keep it stopped.

The last limit comes from how the tracer attaches to the processes it starts, which the builds of kizz rely on too.

## Exit statuses

* The program's exit code, or 128 plus the number of the signal that killed it, such as 143 for `SIGTERM`.
* 127 when the program is not found.
* 126 when it is found but cannot be run or started.
* 125 when kizz itself fails after it accepted the command line: the tracer failed, or the report could not be written.
* 2 for a command-line error, such as a missing `--output` or a missing directory for it; then nothing runs.

## What it does not do

`kizz execute` deliberately does not:

* cache command results;
* write or change `kizz.md` files;
* infer the dependency declarations of other build systems, or migrate a build;
* know the meaning of any particular compiler, language or tool;
* sandbox the program or restrict what it may access;
* capture what the program read or wrote, only which paths and how;
* replace `strace` as a tool for debugging individual system calls;
* provide a graphical explorer of processes or dependencies.
