+++
title = "Environment variables"
weight = 2
+++

This page lists every environment variable that kizz reads, with what it changes and its default.

## Overview

Each task starts with an empty environment.
This page covers the variables that kizz itself reads, not those that a task gives its commands.

* `KIZZ_CACHE_DIR`, `XDG_CACHE_HOME` and `HOME` locate the cache, and `HOME` also stands for `~` in output and in patterns.
* `PATH` is where `kizz execute` looks up its program.
* `NO_COLOR`, `CLICOLOR`, `CLICOLOR_FORCE` and `TERM` decide whether kizz styles its output, under `--color=auto`.
* The caller's environment is what `env.caller`, `env.inherit` and `kizz execute` read.
* `KIZZ_TEST_SKIP_BLOCKED_SYSCALLS`, `KIZZ_TEST_FAIL_THREAD_STARTS` and `KIZZ_TEST_FAULT` are for the developers of kizz.

Kizz reads each variable from its own environment, which the command was started with.
A task's commands see a variable only when the task program puts it into the task environment.

## The caller's environment

The caller's environment is the environment that kizz itself was started with.

* `kizz build` and `kizz run` give it to each task program,
  where `env.caller(name)` reads one variable and `env.inherit(names...)` copies variables into the task environment.
  These two functions see only the variables whose name and value are valid UTF-8.
  They read a copy that kizz takes once, before the first task program runs, not the process environment itself.
* `kizz execute` gives the program kizz's whole environment, unchanged, whatever bytes it holds,
  and looks the program up in its `PATH`.
* The variables that a task copies with `env.inherit`, or sets from a value of `env.caller`, are part of each command's
  step key, so a change to one runs the commands that received it again.

[The kizz.md page](@/guide/kizz-md.md) explains the task environment.

## `KIZZ_CACHE_DIR`

The cache directory.
When it is set and not empty, it must be an absolute path, and kizz uses that directory as the cache.
A relative path is an error, `KIZZ_CACHE_DIR must be an absolute path`, and the command exits with 2.
When it is unset or empty, kizz uses `$XDG_CACHE_HOME/kizz`, or else `~/.cache/kizz`.

* `kizz build`, `kizz run`, `kizz why`, `kizz cache clean` and `kizz cache reset` read it, and create the directory when
  it does not exist.
* `kizz list`, `kizz execute`, `kizz graph` and `kizz image` never use the cache, so they do not read it.
* Two builds with different cache directories do not wait for each other, because each cache has its own lock.

```sh
KIZZ_CACHE_DIR=/var/tmp/kizz-ci kizz build ci
```

## `XDG_CACHE_HOME`

The base directory for user caches, from the XDG Base Directory Specification.
When `KIZZ_CACHE_DIR` is unset or empty and `XDG_CACHE_HOME` is an absolute path, the cache is `$XDG_CACHE_HOME/kizz`.
An empty or relative value is ignored, as that specification requires, and kizz goes on to `HOME`.
It has no default of its own.

## `HOME`

The home directory of the user.

* The cache: when neither `KIZZ_CACHE_DIR` nor a usable `XDG_CACHE_HOME` gives one, the cache is `$HOME/.cache/kizz`.
  When `HOME` is then unset, empty or relative, the command exits with 2 and the error
  `cannot locate the kizz cache: set KIZZ_CACHE_DIR or XDG_CACHE_HOME to an absolute path, or set HOME to an absolute path`.
* Output: `kizz build`, `kizz run` and `kizz why` show a path under an absolute `HOME` with `~/` in its place.
* Patterns: a pattern of `ignore` or `track` in a `run()` that starts with `~/` starts in an absolute `HOME`.
  Without an absolute `HOME`, such a pattern is an error of that `run()`.

`kizz execute` does not read `HOME` itself; its program gets it with the rest of the environment.

## `PATH`

The directories in which `kizz execute` looks up a program name without `/`, in order.
An empty entry means the current directory, and a relative entry is relative to it.
The first location that holds a regular file with an execute bit is the program,
and every location checked is an observation of the command's first program in the report.
When `PATH` is unset, a name without `/` is not found, and `kizz execute` exits with 127.

`kizz build` and `kizz run` do not use kizz's own `PATH`: `run()` looks a program up in the `PATH` of the task environment,
which a task usually copies with `env.inherit("PATH")`.

```sh
PATH=/usr/bin kizz execute --output=report.txt -- wc -w notes.txt
```

## `NO_COLOR`

Turns color off: when `NO_COLOR` is set, to any value, an empty one included, kizz styles neither of its output streams.
It applies only under `--color=auto`, the default, and `CLICOLOR_FORCE` overrides it.
It is unset by default.

```sh
NO_COLOR=1 kizz build ci
```

## `CLICOLOR`

`CLICOLOR=0` turns color off, as `NO_COLOR` does.
Any other value, and an unset `CLICOLOR`, leave the choice to the other rules.
It applies only under `--color=auto`, and `CLICOLOR_FORCE` overrides it.

## `CLICOLOR_FORCE`

Turns color on: when `CLICOLOR_FORCE` is set to anything but `0`, an empty value included, kizz styles both of its output streams,
also when they go to a file or a pipe.
It wins over `NO_COLOR`, `CLICOLOR` and `TERM`.
It applies only under `--color=auto`: `--color=never` turns color off whatever it says.
It is unset by default, which is the same as `0`.

```sh
CLICOLOR_FORCE=1 kizz build ci | less -R
```

## `TERM`

The type of the terminal.
Under `--color=auto`, kizz styles a stream that is a terminal only when `TERM` is set and is not `dumb`,
unless `CLICOLOR_FORCE` turns color on.
It changes nothing else in kizz's own output: a task's commands see it only when the task copies it into the task environment.

Under `--color=auto`, kizz decides for standard output and standard error apart, from these four variables:
a stream is styled when `CLICOLOR_FORCE` is set and not `0`, or when the stream is a terminal, `NO_COLOR` is unset,
`TERM` is set and not `dumb`, and `CLICOLOR` is not `0`.
A value that is not valid UTF-8 counts as unset for each of the four.
These rules belong to the terminal library that kizz uses, `console`.

## `KIZZ_TEST_SKIP_BLOCKED_SYSCALLS`

For developers: only tests of the tracer read it, never the `kizz` command.
They are the tracer's system-call tests, in `crates/kizz-trace-linux/tests/system_calls.rs`, and the tracer conformance test,
in `crates/kizz/tests/tracer_coverage.rs`.
They check that the tracer sees certain system calls and system-call numbers that the tests make.
Where a seccomp filter installed before kizz's own refuses such a call, the tracer never sees it, and the test fails.
When the variable is `1`, the tests skip the checks of each call that the tracer never saw,
and print a line that starts with `NOTICE:` and names the calls; any other value, or none, keeps the tests strict.
A call that the tracer saw is checked in full either way.

## `KIZZ_TEST_FAIL_THREAD_STARTS`

For developers: a number N that makes the first N attempts to start a thread in the `kizz` process fail with `EAGAIN`,
so that tests can show how kizz waits, tries again, and finally fails with an error instead of a panic.
The waits of the attempts that it fails are recorded instead of spent.
Only builds with debug assertions read it, as the test builds of kizz have, and a release build ignores it.
A value that is not a whole number from 0 to 4294967295 is ignored, and so is the variable when it is unset, the default.

## `KIZZ_TEST_FAULT`

For developers: when it is set, to any value, kizz reads a page of memory that it mapped without access,
right after it installs its handler of stack overflows.
That fault lies outside every stack guard, so the handler hands it on, and kizz dies of `SIGSEGV` without an error of its own.
The end-to-end test of stack overflows sets it to show that kizz never swallows a real crash.
Only builds with debug assertions read it, as the test builds of kizz have, and a release build ignores it.
