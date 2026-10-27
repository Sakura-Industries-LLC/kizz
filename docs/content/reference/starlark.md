+++
title = "Starlark API"
weight = 5
+++

This page describes every global, member and function that kizz adds to Starlark, with its parameters,
what it tracks as inputs and outputs, and its side effects.
The [kizz.md format](@/reference/kizz-md.md) describes the blocks that hold task programs,
and [the kizz.md guide](@/guide/kizz-md.md) explains how to write them.

## Overview

A task program is Starlark, a small dialect of Python.
The language itself, its types, statements, operators and built-in functions,
is described by the [Starlark language specification][starlark-spec], not by this site.
Kizz runs programs with the `starlark` crate, version 0.14.2, in its [extended dialect][starlark-dialect].
Besides the standard language, that dialect allows `if` and `for` statements at the top level of a file,
keyword-only parameters after `*`, and type annotations.

The block's source plus every module it loads is the *task program*.
Kizz parses the block with blank lines before it, so the line numbers in Starlark errors are the lines of the `kizz.md`.
Errors in a module name the module's absolute path and its own lines.

Kizz adds these names to Starlark's standard built-in functions:

* [`run()`](@/reference/starlark.md#run) runs a command and returns its result.
* [`env`](@/reference/starlark.md#env-1) reads the caller environment and builds the task environment.
* [`fs`](@/reference/starlark.md#fs) reads and writes files and JSON, and inspects the file system.
* [`task`](@/reference/starlark.md#task) describes the running task.
* [`json`](@/reference/starlark.md#json) converts between values and JSON text, from the `starlark` crate.
* [`print()`](@/reference/starlark.md#print) prints a line with the task's prefix, from the `starlark` crate.

[`fail()`](@/reference/starlark.md#fail) and [`load()`](@/reference/starlark.md#load) are part of the standard language,
and their entries say what they do in a task.
The other globals are those of the `starlark` crate's standard environment, such as `len`, `sorted`, `getattr` and `type`.

A program runs in one of two places:

* In a task: the block runs from its first line to its last, in every pass of every build that selects the task,
  and once under `kizz run`.
  Every function on this page works there.
* While a module loads: each `load()` evaluates the module's top-level code once for the task that loads it, outside the task.
  There, `fs`, `env.read()`, `task`, `json`, `print()` and `fail()` work,
  and `run()` and every other `env` function fail the task with an error such
  as `run() cannot be called while the module /path/lib.star is being loaded, outside any task`.
  Functions that a module defines and a task calls act on that task, so their `run()`, `env` and `fs` calls belong to it.

Every file access that a program makes, in the block or while a module loads, is recorded against its task.
Kizz uses these records to learn the order of tasks, to list the paths that a failed task looked for and did not find,
and for the report of `kizz run --output`.
They do not decide whether a command runs: each `run()` call has its own cache check.
The program runs again in every pass, so its own reads always see current data.

## `run()`

```starlark
result = run(
    "cargo", "build", "--release",
    stdin = None,
    env = None,
    cwd = None,
    capture = "split",
    stream = False,
    always = False,
    ignore = [],
    track = [],
    timestamps = False,
    machine = [],
    expect = None,
)
```

Runs one command, a *step* of the task, traces it, and returns its result.
The positional arguments are the program and its arguments, and every other parameter is a keyword argument with a default.
`run()` works only in a task, not while a module loads.

Each call goes through these stages:

1. Kizz builds the step's specification from the arguments and the task environment.
2. It derives the *step key*, a BLAKE3 hash of the task program's hash and of the values that each entry below marks as
   part of the step key, plus the *occurrence number*: how many identical commands came earlier in the same run of the task.
3. It checks the cache.
   A step whose record holds a cached result, and whose recorded paths and named machine state are unchanged, is a cache hit:
   `run()` returns the recorded exit status and output, and nothing runs.
4. Otherwise it looks the program up, runs it through the tracer, and records every path that the command's process tree touched,
   with the state of each path when the command finished.
   In a later pass of the same build, kizz returns the earlier result instead, without running the command.
   This holds for an `always` step, every step of a `cache=never` task and every step under `kizz run --no-cache`.
   It also holds for a result that was not cached, while no path that its run touched has changed.
   Such a result comes back with `cached` set, and a command that could not start fails the task again with the same error.
5. It stores the step record under the step key, and returns the result.

The [caching guide](@/guide/caching.md) explains the cache check,
and the [caching controls](@/reference/kizz-md.md#caching-controls) list every option that changes it.

A step's record holds every parameter as the call gave it, the program that ran, the exit status, the raw captured output,
the paths with their states, the machine state, the network connections and the unknown system calls.
Explanations use it to say what changed, and `kizz why` shows it.

### `argv`

The positional arguments: the program, then its arguments, each a string.

* Type: one or more strings, given positionally, such as `run("go", "test", "./...")`.
  There is no default: `run()` without a program fails the task with `run() needs a program: run("program", "argument", ...)`.
* What it does: kizz runs the program directly, without a shell, with these arguments.
  Use `run("sh", "-c", "...")` for shell features such as pipes.
  The program is looked up as [Program lookup](@/reference/starlark.md#program-lookup) describes.
* Step key: part of it, word by word, so a changed argument gives another step key.
* Tracks: the arguments themselves, through the step key.
  A program found through `PATH` is a dependency, with every location the lookup checked.
* Side effects: those of the command, which kizz does not limit.

### `stdin`

The text for the command's standard input.

* Type and default: a string, or `None`, the default.
* What it does: with a string, the command reads that text, encoded as UTF-8, from its standard input.
  With `None`, its standard input is `/dev/null`.
  An empty string and `None` are different values.
* Step key: part of it, so different text gives another step key.
* Tracks: the text, through the step key.
* Side effects: none of its own.

### `env`

Variables for this one command, applied to the task environment.

* Type and default: a dict from strings to strings or `None`, or `None`, the default.
* What it does: the command's environment is the task environment with these entries applied: a string sets a variable,
  and `None` removes it.
  The task environment itself does not change, so the next `run()` sees it as before.
* Step key: the command's complete environment, after these entries, is part of it.
  So adding, removing or changing any variable that the command receives gives another step key.
* Tracks: every variable that the command receives, with its value, through the step key.
* Side effects: none of its own.

The [`env`](@/reference/starlark.md#env-1) global builds the task environment, which starts empty.

### `cwd`

The command's working directory.

* Type and default: a string, or `None`, the default, which means the task directory, the directory of the task's `kizz.md`.
* What it does: a relative path is resolved against the task directory, and an absolute path stands for itself.
  A `PATH` entry that is empty or relative, and a program name that holds `/`, are resolved against this directory.
* Step key: the absolute working directory is part of it, so the same command in another directory has another step key.
  A task's steps use the absolute task directory, so a project moved to a new path gets other step keys,
  and its commands run again unless the cache holds valid records made at that path.
* Tracks: the directory, through the step key.
  The `filesystem` and `disk` parts of [`machine`](@/reference/starlark.md#machine) describe the file system that holds it.
* Side effects: none of its own.
  A directory that does not exist keeps the command from starting, which fails the task.

### `capture`

How the command's output is captured.

* Type and default: the string `"split"`, the default, or `"merged"`.
  Any other value fails the task with `run(): capture must be "split" or "merged", not "both"`, naming the value.
* What it does: output is always captured.
  With `"split"`, standard output and standard error are kept apart, and the result has `stdout` and `stderr`.
  With `"merged"`, both go through one pipe, so their order is exact, and the result has `output`.
* Step key: part of it.
* Tracks: the captured output is stored in the step record, as raw bytes, and replayed on a cache hit.
* Side effects: none of its own.

### `stream`

Whether the command's output is also shown as it comes.

* Type and default: a bool, `False` by default.
* What it does: with `True`, kizz echoes the output live while the command runs.
  Its standard error goes to kizz's standard error, and its standard output or merged stream to kizz's standard output.
  The output is captured either way.
  In a build, a cache hit shows nothing, because nothing runs; the program still receives the recorded output.
  `kizz run` shows every command's output as if each `run()` had `stream = True`, and replays the output of a cache hit.
* Step key: not part of it, because it changes what kizz shows, not what the command does.
* Tracks: nothing; the step record notes the value.
* Side effects: output on kizz's standard output and standard error.

### `always`

Whether the command runs in every build that reaches it, even when its cache entry is valid.

* Type and default: a bool, `False` by default.
* What it does: with `True`, the command runs at most once per build, when a pass first reaches it, and without a cache check.
  Later passes of the same build return that run's result again, with `cached` set, without running the command.
  Use it for a command whose effect kizz cannot see, such as an upload, a download or a notification.
  Its reason is "marked `always`" when its step key has a record.
  Without one, it gets the reasons of any step key without a record: "never run",
  or what changed since the task's step at that position last ran.
* Step key: not part of it.
* Tracks: everything a run tracks: the command's paths are recorded as for any step,
  so steps that read the files it writes stay cached when it rewrites them with the same content.
* Side effects: those of the command, once in each build.

A task marked [`cache=never`](@/reference/kizz-md.md#cache-never) runs every command as if it had `always = True`,
and so does `kizz run --no-cache`.

### `ignore`

Paths that are not recorded as dependencies of this step.

* Type and default: a list or tuple of pattern strings, empty by default.
* What it does: when the command finishes, every path that a pattern matches is left out of the step's record.
  An ignored path never makes the step run again.
  So ignore only what the command can rebuild by itself, such as a tool's own cache.
  Kizz counts the ignored accesses, and `kizz why` reports the count for each step.
  Kizz adds no pattern of its own.
* Patterns are matched against absolute paths:
    * A pattern that starts with `re:` is a regular expression, in the syntax of the Rust `regex` crate,
      which matches anywhere in the path unless it is anchored with `^` and `$`.
    * Any other pattern with `*`, `?`, `[` or `{` is a glob: `*` and `?` stay within one path component,
      `**` crosses components, `[...]` matches one character of a set, and `{a,b}` matches either alternative.
    * Every other pattern is a path, which matches exactly that path.
    * A relative path or glob is anchored at the task directory, and an absolute one stands for itself.
    * A leading `~/` means the caller's home directory, the absolute `HOME` that kizz was started with.
    * Paths are compared as the tracer reports them, normalized, so `a/./b` and `a//b` match `a/b`.
* Step key: part of it, as written, so changing a pattern gives another step key.
* Tracks: the patterns, through the step key, and the number of ignored accesses, in the step record.
* Side effects: none.
* Errors: an invalid glob, an invalid regular expression, and a `~/` pattern when `HOME` is unset or not absolute fail the task.
  Kizz checks the patterns of every call, before its cache check, and stores no record for such a call.

Starlark variables make reusable sets of patterns:

```starlark
GO_CACHE = ["~/.cache/go-build/**"]

run("go", "build", "./...", ignore = GO_CACHE, expect = [0])
```

### `track`

Paths under `/proc` and `/sys`, or that symbolic links lead there, whose content counts as an input of this step.

* Type and default: a list or tuple of pattern strings, empty by default,
  in the forms of [`ignore`](@/reference/starlark.md#ignore).
* What it does: kizz records every access to `/proc` and `/sys` that `ignore` does not leave out,
  but by default never compares those paths, because they are kernel interfaces that change on every read.
  The same holds for a path that symbolic links lead into them, such as `/dev/stdin` or `/dev/fd/3`:
  the object that it reaches is not compared, while the links on the way are compared by their targets.
  A path of either kind that the command touched and that a `track` pattern matches is recorded with its state,
  and a regular file's content is read and hashed again at that time.
  The pattern is matched against the path that the command used, such as `/dev/stdin`, not against where it leads.
  The cache check reads it again and compares it, so a change runs the step again.
* A pattern has no effect on a path that is not under `/proc` or `/sys` and does not lead there through symbolic links,
  which is compared anyway, or on a path that `ignore` matches.
  Per-process paths such as `/proc/self` and `/dev/stdin` describe kizz when it checks the cache, not the command.
  So tracking them is rarely useful.
* Step key: part of it, as written.
* Tracks: the content and state of each matching path that the command touched, under `/proc` or `/sys` or leading there.
* Side effects: none.
* Errors: as for `ignore`.

```starlark
run("./configure", track = ["/proc/cpuinfo"], expect = [0])
```

### `timestamps`

Whether a new timestamp of a used file or directory is a change.

* Type and default: a bool, `False` by default.
* What it does: by default, a regular file that the step read or ran is unchanged when its content is the same,
  even when its modification or change time is new, and the times of a directory are not compared.
  With `True`, a different modification or change time of a regular file that the step read or ran, or of a directory that it read,
  listed or checked, is a change of its own, even when the content is the same.
  A regular file that the step only checked or wrote has no hash, so all its metadata, its times included, is compared,
  with or without `timestamps`; a directory that the step only wrote is not compared by its times.
  Use it for tools such as `tar` and `zip`, which store the timestamps of what they pack.
  Access times, and the times of a symbolic link itself, are not compared.
* Step key: part of it.
* Tracks: the times of the paths that the step used, which the record holds in any case.
* Side effects: none.

### `machine`

The parts of the machine state that count as an input of this step.

* Type and default: a list or tuple of part names, empty by default.
* What it does: kizz records the whole machine state, as the command starts, with every step that runs.
  A part named here is compared in the cache check, for the record's working directory,
  and a step runs again when a value of a named part differs from the recorded one.
  The explanation names each changed value, with its old and new value.
  A step key's machine state is compared at most once per build, and not after its step ran in that build,
  so a part that keeps changing runs the step at most once per build.
* The parts:
    * `kernel`: the kernel release and version;
    * `limits`: every resource limit, soft and hard;
    * `affinity`: the CPUs that the command may run on;
    * `memory`: the total and the available memory;
    * `filesystem`: the type and mount flags of the file system that holds the working directory;
    * `disk`: the size and free space of that file system.
* Available memory and free space change on almost every build, so a step that names `memory` or `disk` runs on almost
  every build.
* Step key: the names are part of it, sorted and without repeats, so their order does not matter.
* Tracks: the named parts' values, which the record holds with the rest of the machine state.
  `kizz why` shows the parts that a step names, and every part with `--verbose`.
* Side effects: none.
* Errors: an unknown name fails the task, with
  `run(): machine has no part "cpu"; the parts are kernel, limits, affinity, memory, filesystem and disk`.

The host name, the user's credentials, the umask, the machine architecture and the CPU model are not per step:
they are the cache's identity, which the [caching guide](@/guide/caching.md#the-cache-identity) describes.

### `expect`

The exit codes that are acceptable, so that any other exit fails the task.

* Type and default: `None`, the default, or a list or tuple of integers.
* What it does: with `None`, `run()` returns whatever the exit status, and the program decides what it means.
  With a list, the task fails when the command exits with a code that is not in it, or when a signal killed it.
  An empty list accepts no exit.
  The check applies to a result from the cache too, so such a step fails again on a cache hit.
* When it fails the task, kizz prints the command, its exit status and its captured output, then the Starlark error:

```text
error: kizz.md:33 (Expect) failed:
  command `sh -c echo oops; exit 2`: exit code 2, but `expect` accepts exit code 0 or 1
  --- stdout ---
  oops
  --- stderr ---
  Traceback (most recent call last):
    * kizz.md:35, in <module>
        run("sh", "-c", "echo oops; exit 2", expect = [0, 1])
  error: run(): command `sh -c echo oops; exit 2`: exit code 2, which `expect` does not accept
    --> kizz.md:35:1
     |
  35 | run("sh", "-c", "echo oops; exit 2", expect = [0, 1])
     | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
     |
```

* A step without `expect` whose command exits with a non-zero code or is killed by a signal is *unchecked*,
  unless the program checks its result, as [Checking a result](@/reference/starlark.md#checking-a-result) describes.
* Step key: not part of it, because it changes what the program does with the result, not what the command does.
* Tracks: nothing; the step record notes the value.
* Side effects: none of its own.

### Program lookup

* A program name without `/` is looked up in the `PATH` of the command's environment: the task environment with `env` applied.
  Kizz's own `PATH` is not used.
* Without `PATH` in that environment, a bare name fails the task, and kizz stores no record for such a call.
  The message says that a program name without `/` is looked up in the task environment's `PATH`,
  and that the task environment has none: "set it with `env.set("PATH", ...)` or inherit it with `env.inherit("PATH")`".
* The directories of `PATH` are tried in order, as `execvp` tries them.
  An empty entry means the working directory, and a relative entry is resolved against it.
* The first location that holds a regular file with an execute bit is the program.
* Every location checked is recorded as a dependency of the step, so a program that appears earlier in `PATH` runs it again.
* A name with `/` is not looked up: it is resolved against the working directory.
* Lookup happens only when the step runs, not on a cache hit.

### Failures

These fail the task at the `run()` call, with a Starlark error and the stack:

* a call outside a task, while a module loads;
* a call without a program, an invalid `capture`, or an unknown `machine` part;
* an invalid `ignore` or `track` pattern;
* a bare program name without `PATH`;
* a program that is not found, or not executable;
* a command that cannot start for another reason;
* an exit that `expect` does not accept.

A program that is not found or not executable is recorded like any other step, with every location checked,
so the step fails again from the cache until a checked location changes, and it runs again when the program appears.
A command that cannot start for another reason, such as one whose `cwd` does not exist, has a result that is not cached.
Within the build it runs again only when a path that it touched changes, and a later build runs it again.

A program that is not found names every location checked:

```text
  error: run(): program `tool` was not found in the task environment's PATH; checked /opt/bin/tool, /opt/other/tool
```

Commands that finished before a failure keep their cache entries.

### The result, `run_result`

`run()` returns a value of the Starlark type `run_result`, whose attributes depend on the capture mode: `rc`, `signal`,
`stdout` and `stderr`, and `cached`, with `"split"`; `rc`, `signal`, `output` and `cached`, with `"merged"`.
Reading an attribute that the mode does not have is a Starlark error.

* `dir()` lists the attributes, and `hasattr` and `getattr` work on them.
* Two results are equal when their exit status, output and `cached` are equal.
* `print()` and `str()` show every attribute, such as
  `run_result(rc=0, signal=None, stdout="out\n", stderr="err\n", cached=False)`.
* `json.encode()` and `fs.write_json()` encode a result as an object of its attributes, in the order above,
  such as `{"rc":0,"signal":null,"stdout":"out\n","stderr":"err\n","cached":false}`.
* A result exists only while its task runs.

Reading an attribute tracks nothing.
Reading `rc` or `signal` checks the result, which is its one side effect.

#### `rc`

The exit code, an integer, or `None` when a signal killed the command.
It takes no parameters and tracks nothing.
Reading it, as an attribute or with `getattr`, checks the result, which is its one side effect.

#### `signal`

The number of the signal that killed the command, an integer, or `None` when it exited.
It takes no parameters and tracks nothing.
Reading it, as an attribute or with `getattr`, checks the result, which is its one side effect.

#### `stdout`

The command's standard output, a string, with `capture = "split"`.
The bytes are decoded as UTF-8, with invalid bytes replaced by U+FFFD; the cache keeps the raw bytes.
It takes no parameters, tracks nothing and has no side effect: reading it does not check the result.

#### `stderr`

The command's standard error, a string, with `capture = "split"`, decoded as `stdout` is.
It takes no parameters, tracks nothing and has no side effect: reading it does not check the result.

#### `output`

Both output streams in the order they were written, a string, with `capture = "merged"`, decoded as `stdout` is.
It takes no parameters, tracks nothing and has no side effect: reading it does not check the result.

#### `cached`

Whether kizz returned a recorded result instead of running the command for this call: a bool.
It is `True` for a cache hit, and for a result that the same build returns again, such as that of an `always` step in a later pass.
It takes no parameters, tracks nothing and has no side effect: reading it does not check the result.

### Checking a result

Without `expect`, a non-zero exit or a signal does not fail the task at its `run()` call: the program decides what it means.
A step's exit is *unchecked* when the command exited with a non-zero code or was killed by a signal, its `run()` had no `expect`,
and the program, by the time it ended, never checked the result.
The program checks a result when it:

* reads `rc` or `signal`, as an attribute or with `getattr`;
* encodes the result with `json.encode()` or `fs.write_json()`, also inside an encoded list or dict.

`hasattr`, `dir()`, printing the result, and reading `stdout`, `stderr`, `output` or `cached` do not check it.
A result from the cache counts like any other.

Once a build settles, kizz warns about each unchecked exit in the results that stand, those of each task's latest run,
and repeats the warning in the summary.
The warning names the task, the command and how it ended, and says that the program never read `rc` or `signal`.

`kizz run` warns once the run ends.
[`kizz build --fail-unchecked`](@/reference/command-line.md#fail-unchecked)
and `kizz run --fail-unchecked` make each one fail its task instead.

## `env`

Reads the caller environment and dotenv files, and changes the task environment.

* The *caller environment* is the environment that kizz was started with, without variables whose name or value is not UTF-8.
  `env.caller()` and `env.inherit()` read it; no other function does, and `run()` does not pass it on.
* The *task environment* starts empty in every run of a task program, and only these functions change it.
  Each `run()` passes it to its command, with the call's [`env`](@/reference/starlark.md#env) entries applied.
* The command's exact environment is part of its step key, so every variable that reaches a command is tracked through it.
  Reading the caller environment is not recorded as an input of its own.
* `env.read()` works while a module loads; every other `env` function works only in a task, and fails the task there
  with an error such as `env.set() cannot be called while the module /path/lib.star is being loaded, outside any task`.

```starlark
env.inherit("PATH", "HOME")
env.load_file(".env", defaults = {"GOFLAGS": "-trimpath"}, required = False)
env.set("CGO_ENABLED", "0")
run("go", "build", "./...", env = {"GOFLAGS": None}, expect = [0])
```

### `env.caller()`

`env.caller(name, default = None)` returns the value of the variable `name` in the caller environment,
or `default` when it is unset.

* Parameters: `name`, a string, given positionally; `default`, any value, `None` by default.
* Returns: a string, or `default`.
* Tracks: nothing; a value that the program passes to a command is tracked there.
* Side effects: none; the task environment does not change.

### `env.inherit()`

`env.inherit(name, ...)` copies the named variables from the caller environment into the task environment.

* Parameters: any number of names, strings, given positionally.
* Returns: `None`.
* A name that the caller environment does not set is skipped, and does not remove a variable of the task environment.
* Tracks: nothing at the call; each copied variable is part of the step key of every later command that receives it.
* Side effects: sets each copied variable in the task environment, replacing its value.

### `env.load_file()`

`env.load_file(path, defaults = {}, required = True)` reads a dotenv file, fills in `defaults` for the variables it does not set,
adds the result to the task environment, and returns it.

* Parameters: `path`, a string, given positionally, relative to the task directory unless absolute;
  `defaults`, a dict from strings to strings, empty by default; `required`, a bool, `True` by default.
* Returns: a dict of the variables that it added: the defaults, overridden by the file's variables.
* A variable of the file replaces a default of the same name, and both replace a variable that the task environment already has.
* With `required = False`, a missing file means that only the defaults apply.
* Tracks: the file, as a read of the task, or as a probe when it is missing.
  Its variables reach commands through the task environment, so they are part of their step keys.
* Side effects: sets each returned variable in the task environment.
* Errors: a missing file with `required = True`, a file that cannot be read, a syntax error of the file,
  and a default whose name is empty or holds `=`, or whose name or value holds a NUL character.
  A syntax error names the file and the line:

```text
  error: env.load_file(): /work/bad.env:2: expected NAME=value, a comment starting with #, or a blank line, not `not an assignment`
```

### `env.read()`

`env.read(path)` reads a dotenv file into a dict, without changing the task environment.

* Parameters: `path`, a string, given positionally, relative to the task directory unless absolute.
* Returns: a dict of the file's variables.
* Tracks: the file, as a read of the task, or as a probe when it is missing.
* Side effects: none.
* Errors: a file that is missing or cannot be read, and a syntax error of the file.
* It also works while a module loads.

### `env.set()`

`env.set(name, value)` sets the variable `name` of the task environment to `value`.

* Parameters: `name` and `value`, strings, given positionally.
* Returns: `None`.
* Tracks: nothing at the call; the variable is part of the step key of every later command that receives it.
* Side effects: sets the variable, replacing its value.
* Errors: a name that is empty or holds `=`, and a name or value that holds a NUL character.

### `env.update()`

`env.update(values)` sets every variable of the dict `values` in the task environment.

* Parameters: `values`, a dict from strings to strings, given positionally.
* Returns: `None`.
* Tracks: nothing at the call, as for `env.set()`.
* Side effects: sets each variable, replacing its value.
* Errors: as for `env.set()`; when one entry is invalid, none is set.

### `env.unset()`

`env.unset(name)` removes the variable `name` from the task environment, if it is set.

* Parameters: `name`, a string, given positionally.
* Returns: `None`.
* Tracks: nothing at the call; the variable's absence is part of the step key of every later command.
* Side effects: removes the variable.

### `env.get()`

`env.get(name, default = None)` returns the value of the variable `name` in the task environment, or `default` when it is unset.

* Parameters: `name`, a string, given positionally; `default`, any value, `None` by default.
* Returns: a string, or `default`.
* Tracks: nothing.
* Side effects: none.

### `env.dict()`

`env.dict()` returns the task environment as a new dict.

* Parameters: none.
* Returns: a dict from names to values, sorted by name; changing it does not change the task environment.
* Tracks: nothing.
* Side effects: none.

### `env.clear()`

`env.clear()` empties the task environment.

* Parameters: none.
* Returns: `None`.
* Tracks: nothing at the call; the next commands receive an empty environment, which is part of their step keys.
* Side effects: removes every variable of the task environment.

### Dotenv files

`env.load_file()` and `env.read()` read the same format:

* one assignment per line, `NAME=value`, optionally after `export` and a space or tab;
* blank lines, and lines whose first character other than whitespace is `#`, are skipped;
* a name starts with an ASCII letter or `_`, and continues with ASCII letters, digits and `_`, with no space before `=`;
* an unquoted value is the rest of the line, without the whitespace around it, so a `#` after it is part of the value;
* a value in single quotes keeps every character as written, up to the closing quote;
* a value in double quotes supports the escapes `\n`, `\t`, `\"` and `\\`, and no other;
* only whitespace may follow a closing quote;
* `$` is an ordinary character, because there is no `${VAR}` expansion, which would silently pull values from elsewhere;
* when a file assigns a name twice, the later value wins.

Any other line is a syntax error that names its line, and it fails the task.
Bytes that are not valid UTF-8 are replaced before the file is parsed.

## `fs`

Reads and writes files, and inspects the file system.

* A path is relative to the task directory, also in a module and while a module loads; an absolute path stands for itself.
  Paths outside the project are allowed, because kizz is not a sandbox.
* Every access is recorded against the task, with its kind: a read of a file's content is a `read`,
  an existence check or a path found missing is a `probe`, a directory read is a `list`, and a write is a `write`.
* A write happens only when the content differs, so an unchanged file keeps its timestamps.
  A write that changes a file is a *change*: the task becomes the file's last writer, which orders the tasks of later builds,
  and the pass did work, so the build checks every selected task in another pass.
  `kizz run` runs no further pass.
* Text is UTF-8: bytes that are not valid UTF-8 are replaced with U+FFFD.
* Every `fs` function also works while a module loads.

### `fs.read()`

`fs.read(path)` returns the text of a file.

* Parameters: `path`, a string, given positionally.
* Returns: a string, with invalid UTF-8 replaced.
* Tracks: a read of the file, or a probe when it is missing.
* Side effects: none.
* Errors: a file that is missing or cannot be read, such as a directory.

### `fs.write()`

`fs.write(path, text)` writes `text` to a file, creating its parent directories, unless the file already holds exactly `text`.

* Parameters: `path` and `text`, strings, given positionally.
* Returns: `None`.
* Tracks: a write of the file, whether or not its content changed, and a write of each parent directory that it created.
* Side effects: creates the missing parent directories, and replaces the file's content when it differs.
  Such a write is a change, which makes the task the file's last writer and gives the build another pass.
* Errors: a directory or file that cannot be created or written.

### `fs.read_json()`

`fs.read_json(path)` returns the JSON value in a file as a Starlark value.

* Parameters: `path`, a string, given positionally.
* Returns: `None`, a bool, an int for an integer that fits in 64 bits, a float for any other number, a string, a list or a dict.
* Tracks: a read of the file, or a probe when it is missing.
* Side effects: none.
* Errors: a file that is missing or cannot be read, a file that does not hold valid JSON, and a number out of range.

### `fs.write_json()`

`fs.write_json(path, value, indent = 2)` writes `value` to a file as stable JSON, unless the file already holds exactly that text.

* Parameters: `path`, a string, and `value`, given positionally; `indent`, an int of 0 or more, 2 by default.
* Returns: `None`.
* The text is stable: object keys sorted by their UTF-8 bytes, each element and member on its own line,
  indented by `indent` spaces per level, `": "` between a key and its value, `[]` and `{}` for empty arrays and objects,
  and a trailing newline.
  So the same value always gives the same bytes, and an unchanged value leaves the file alone.
* A `run_result` is written as an object of its attributes, which checks the result.
* Tracks: a write of the file, whether or not its content changed, and a write of each parent directory that it created.
* Side effects: as for `fs.write()`.
* Errors: a value that has no JSON form, such as a function, a negative `indent`,
  and a directory or file that cannot be created or written.

```starlark
fs.write_json("out/data.json", {"b": [1, 2], "a": True})
```

This writes:

```json
{
  "a": true,
  "b": [
    1,
    2
  ]
}
```

### `fs.exists()`

`fs.exists(path)` returns whether a path exists, following symbolic links.

* Parameters: `path`, a string, given positionally.
* Returns: a bool; a path that cannot be checked, such as one behind a directory without permission, gives `False`.
* Tracks: a probe of the path, so the path appearing or disappearing is recorded for the task.
* Side effects: none.

### `fs.list()`

`fs.list(path = ".")` returns the names of the entries of a directory, sorted.

* Parameters: `path`, a string, given positionally, the task directory by default.
* Returns: a list of names, without `.` and `..`, hidden names included, with invalid UTF-8 replaced.
* Tracks: a list of the directory, or a probe when it is missing.
* Side effects: none.
* Errors: a directory that is missing or cannot be read.

### `fs.glob()`

`fs.glob(pattern)` returns the paths that match a glob pattern, sorted.

* Parameters: `pattern`, a string, given positionally.
* Returns: a list of paths, each written as the pattern's leading directories followed by the matching path below them.
* The pattern is split at its first component that holds a glob character, `*`, `?`, `[`, `]`, `{`, `}` or `\`.
  The components before it name the directory where the search starts, relative to the task directory unless absolute.
  The rest is matched against the paths below that directory.
* `*`, `?`, `[...]` and `{a,b}` stay within one component, and `**` matches any number of components.
  A backslash escapes the character after it.
  Names that start with `.` match like any other.
* Files and directories both match, and symbolic links to directories are followed, without following a link back into
  a directory that the search is in.
* A pattern without glob characters checks its one path, and returns it when it exists.
* A missing starting directory gives an empty list.
* Tracks: a list of every directory that the search read, and a probe of every missing one;
  a pattern without glob characters is a probe of its path.
* Side effects: none.
* Errors: an empty pattern, an invalid pattern, and a directory that cannot be read.

```starlark
for manifest in fs.glob("services/*/package.json"):
    print(manifest)
```

## `task`

Describes the running task.
Its members are values, not functions, and they cannot be changed.
`task` is also available while a module loads, and describes the task that loads it.

None of them is part of the task program's hash: a value reaches a step key only through what the program passes to a command.

### `task.dir`

The task directory, the absolute directory of the task's `kizz.md`, as a string.
It is the directory that relative paths of `run()`, `env` and `fs` are resolved against.
It takes no parameters, tracks nothing and has no side effect.

### `task.file`

The absolute path of the task's `kizz.md`, as a string.
It takes no parameters, tracks nothing and has no side effect.

### `task.line`

The line of the block's opening fence in its `kizz.md`, counting from 1, as an int.
It takes no parameters, tracks nothing and has no side effect.

### `task.name`

The name that the block's [`name=`](@/reference/kizz-md.md#name) option gives, as a string, or `None` when it has none.
Before tasks had names, `task.name` held the heading, which is now `task.heading`.
It takes no parameters, tracks nothing and has no side effect.

### `task.heading`

The task's [heading](@/reference/kizz-md.md#the-heading), the nearest Markdown heading above the block, as plain text,
or `None` when no heading is above it.
It takes no parameters, tracks nothing and has no side effect.

### `task.targets`

The targets on the block, as a list of strings, in the order written and without repeats.
It is an empty list for a task that has only a name.
It takes no parameters, tracks nothing and has no side effect.

### `task.selected`

The requested targets that selected the task, as a list of strings, in the order of the target expression.
For `kizz build ci+check`, a task with both targets has `["ci", "check"]`.
Under `kizz run` it is an empty list, because no target selected the task.
It takes no parameters, tracks nothing and has no side effect.

### `task.args`

The task's arguments, the words after `--` of `kizz run`, as a list of strings.
Under `kizz build` it is an empty list.
The arguments are not part of the task program's hash: they reach commands only through what the program does with them.
An argument that the program passes to a command is part of that command's step key, so a new value gives it another step key.
It takes no parameters, tracks nothing and has no side effect.

```starlark
who = task.args[0] if task.args else "world"
run("echo", "hello,", who, expect = [0])
```

## `json`

The `json` module of the `starlark` crate, which kizz adds to the globals.
It works in a task and while a module loads, tracks nothing, and has no side effect apart from checking the results it encodes.

### `json.encode()`

`json.encode(value)` returns the compact JSON text of a value.

* Parameters: `value`, any value with a JSON form, given positionally.
* Returns: a string; dict keys keep their order.
* A `run_result` is encoded as an object of its attributes, which checks the result.
* Tracks: nothing.
* Side effects: none, apart from checking each `run_result` that it encodes.
* Errors: a value that has no JSON form, such as a function.
* Use `fs.write_json()` to write a file in kizz's stable form.

### `json.decode()`

`json.decode(text)` returns the Starlark value of JSON text.

* Parameters: `text`, a string, given positionally.
* Returns: `None`, a bool, a number, a string, a list or a dict.
* Tracks: nothing.
* Side effects: none.
* Errors: text that is not valid JSON.

## `print()`

`print(value, ...)` prints its arguments as one line of the task's output.

* Parameters: any number of values, given positionally.
  Each is converted as `str()` converts it, and they are joined with single spaces.
* Kizz splits the text at each newline, and prints each line to its standard output with a prefix:
  the task's name, else its heading, else its `kizz.md` and line, then `: `.
* It works in a task and while a module loads, with the prefix of the task that loads the module.
* The program runs in every pass, so its lines are printed again in every pass, also when every command is cached.
* Tracks: nothing; printed lines are not stored.
* Side effects: output on kizz's standard output.

```text
show: run_result(rc=0, signal=None, output="out\nerr\n", cached=False)
```

See the [Starlark specification][starlark-print] for the standard function.

## `fail()`

`fail(value, ...)` fails the task.

* Parameters: any number of values, given positionally, joined with spaces into the message:
  strings as they are, other values as their `repr()`.
* The task fails at once, and kizz prints the message with the Starlark stack:

```text
error: kizz.md:27 (Fail) failed:
  Traceback (most recent call last):
    * kizz.md:28, in <module>
        fail("the version is", 3, None)
  error: fail: the version is 3 None
    --> kizz.md:28:1
     |
  28 | fail("the version is", 3, None)
     | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
     |
```

* While a module loads, it fails the task that loads the module.
* A failed task does not stop the build, and the commands that finished before the failure keep their cache entries.
* Tracks: nothing.
* Side effects: the task fails, and the build goes on with the other tasks.

See the [Starlark specification][starlark-fail] for the standard function.

## `load()`

`load("path/to/lib.star", "name", alias = "name")` imports names from a Starlark module file.
It is a statement of the language, which the [Starlark specification][starlark-load] describes.

* The path is relative to the directory of the file that holds the `load`: the task directory for the block,
  and the module's own directory for a module.
  An absolute path stands for itself.
* Each module is read and evaluated once per run of a task program, outside the task, with the same globals,
  and every `load` of the same path in that run shares it.
* A module may load other modules; a module that loads itself, directly or indirectly, fails the task.
* Tracks: each module file, as a read of the task, or as a probe when it is missing.
  The path and the source of every module that the block loads, directly or indirectly, are part of the task program's hash,
  and so of every step key of the task.
  Editing a module, or loading it from another path, gives every task that loads it other step keys,
  so each of their commands runs again unless a record under its new key is still valid, as one from an earlier version can be.
  The accesses of a module's top-level code, through `fs` and `env.read()`, are recorded against the task.
* Side effects: those of the module's top-level code, such as files that `fs.write()` changes and lines that `print()` prints.
* Errors: a module that cannot be read, a module with a syntax error, a load cycle, a name that the module does not export,
  and an error of the module's top-level code, such as a call of `run()`, each fail the task.

```starlark
load("../build/common.star", "go_build")

env.inherit("PATH", "HOME")
go_build("./cmd/server", output = "dist/server", version = "1.0.0")
```

[starlark-spec]: https://github.com/bazelbuild/starlark/blob/master/spec.md "The Starlark language specification"
[starlark-dialect]: https://docs.rs/starlark/0.14.2/starlark/syntax/struct.Dialect.html "Dialect, in the starlark crate"
[starlark-print]: https://github.com/bazelbuild/starlark/blob/master/spec.md#print "print, in the Starlark language specification"
[starlark-fail]: https://github.com/bazelbuild/starlark/blob/master/spec.md#fail "fail, in the Starlark language specification"
[starlark-load]: https://github.com/bazelbuild/starlark/blob/master/spec.md#load-statements "load, in the Starlark specification"
