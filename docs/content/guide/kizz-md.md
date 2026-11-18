+++
title = "The kizz.md file"
weight = 3
+++

This page explains the `kizz.md` file in detail: how kizz finds it, how a task is written and tagged, what a task program can do,
and how kizz orders, repeats and fails tasks.
The [kizz.md reference](@/reference/kizz-md.md) and the [Starlark reference](@/reference/starlark.md) list every option,
function and member.

## Discovery

A build, a run and a listing start in the current directory, and never look above it.

* Kizz finds every file named exactly `kizz.md` in the current directory and every directory below it, hidden ones included.
* It does not consult Git or its ignore rules: every `kizz.md` it finds is part of the build, even one that Git ignores.
  Planned: a warning for each `kizz.md` that Git might ignore ([planned features](@/guide/planned.md)).
* Every other Markdown file is documentation, and kizz never reads it.
* Symbolic links to directories are followed.
  A link that would make the walk recurse into one of its own parents is skipped with a warning.
* A `kizz.md` reached through several paths runs once, under the first of those paths in sorted order.
* Files are handled in sorted path order, so discovery is deterministic.

So the starting directory decides which tasks exist: `kizz build ci` in a subdirectory runs only its `ci` tasks and those below.

## Task blocks and fences

A task is a fenced code block whose info string, the text after the opening fence,
starts with the word `starlark` and continues with targets, task options or both:

````markdown
## Lint the sources

```starlark ci check name=lint
env.inherit("PATH", "HOME")
run("cargo", "clippy", "--", "-D", "warnings", expect = [0])
```
````

* Backtick and tilde fences are both recognized, with three or more characters, as Markdown allows.
* A block inside a list item or a block quote counts too.
* A `starlark` block nested inside another fenced block, such as a `markdown` example, is that block's content, not a task.
* The first word must be exactly `starlark`: `Starlark`, `starlark_ci` and indented code blocks are not tasks.
* Blocks in other languages are ordinary Markdown, so a `kizz.md` can document its tasks in prose and other examples.

## Targets: grouping tasks

After `starlark`, every word without `=` is a target.
A target is the name of a group of tasks that people build together, such as `ci`, `check` or `test`.

* `kizz build ci` runs every task that has the target `ci`, in the current directory and below.
* `kizz build ci+check` runs the tasks of either target, and a task that has both runs once.
* `kizz build` without a target is an error that lists the targets that exist.
  A target that selects no task is not an error: kizz warns about it and lists the targets that exist.
* `kizz list ci` lists only the tasks of `ci`.
* A target name uses only letters, digits, `_`, `.` and `-`; any other character is a configuration error.
* A target repeated on one block counts once.

A task can have any number of targets, and many tasks in many `kizz.md` files can share one.

## Task options

After `starlark`, every word with `=` is a task option, in any order among the targets.

* `name=<name>` names the task, so that `kizz run <name>` can run it.
  A name uses the characters of a target name.
  Each name is unique within its `kizz.md`, and the same name in two tasks of one file is a configuration error.
  Tasks in different `kizz.md` files may share a name; [`kizz run`](@/guide/run.md) explains which one it picks.
* `cache=never` makes every command of the task run in every build that reaches it,
  as if each of its `run()` calls had `always = True`.
  `never` is the only value it accepts.
  [Caching](@/guide/caching.md) explains when to use it.

An unknown option, an option given twice, an empty or invalid value, and a word that starts with `=` are configuration errors.
Each names the file, the line, the heading and the option.
A configuration error anywhere under the current directory stops `kizz build`, `kizz run` and `kizz list` with exit status 2.

Task options are not part of the task program, so changing them changes no cache key.
A task with a name and no targets is selected only by `kizz run`, never by `kizz build`.
`kizz list` shows each task's options after its targets, such as `[] name=deploy cache=never`.

## Headings

The nearest Markdown heading above a block is the task's heading.
Its text is the heading's plain text, without the Markdown markup: `` ### `fmt` *check* `` gives `fmt check`.
Both `#` headings and underlined headings count, and a task above every heading has none.

Output names a task by its file, line and heading, as `path/kizz.md:LINE (Heading)`, or `path/kizz.md:LINE` without a heading.
The line is the line of the opening fence, counting from 1.
`print` output carries the task's name, else its heading, else its file and line, as a prefix.
So a short, descriptive heading above each task makes kizz's output easy to read.

## Task identity

A task is identified by the absolute path of its `kizz.md` and its position among the task blocks of that file.
The cache keeps each task's program hash, the commands it ran at each step and its learned order under that identity.
Two tasks are never confused, even when they have the same heading or name in different files.

## Untagged blocks

A `starlark` block with neither targets nor a name is not a task, because nothing could select it.
Kizz warns about it, because it is probably a mistake:

```text
warning: kizz.md:3 (Notes): this starlark block has neither targets nor a name, so it is not a task
```

Its task options are still checked, so a mistake in them is still a configuration error.
It takes no position among the tasks of its file.

## The task program and its modules

Each task is one program in [Starlark][starlark-spec],
which kizz runs with the `starlark` crate's [extended dialect][starlark-dialect],
so that a program can use `if` and `for` at its top level.
Kizz does not run plain lists of commands: the program runs commands, inspects their results and decides what happens next.
Besides the language's own functions, a task program uses these globals:

* `run()` runs a command and returns its result; [caching](@/guide/caching.md) explains its caching options.
* `env` reads the caller environment and builds the task environment.
* `fs` reads and writes files and JSON.
* `task` describes the running task.
* `json.encode` and `json.decode`, from the `starlark` crate, convert between values and JSON text.
* `print()` prints a line with the task's prefix, and `fail()` fails the task.

The block's source plus every module it loads is the *task program*.
`load("path/to/lib.star", "name")` imports names from a Starlark module file.
The path is relative to the file that loads it, and absolute paths are allowed.
Functions from modules act on the task that calls them: their `run()`, `env` and `fs` calls belong to that task.
Calling `run()` while a module is being loaded, outside any task, is an error.

The task program's hash covers the block and every module it loads, directly or indirectly.
The hash is part of the cache key of every command of the task, because the program decides which commands run and in what order.
So editing a task, or a module it loads, runs that task's commands again.

The program runs in every build, from the top, even when all its commands are cached.
A cached `run()` gives the program the recorded exit status and output, so it makes the same decisions without the command.

These members of `task` describe the running task:

* `task.dir`: the directory of its `kizz.md`, the task directory
* `task.file`: the absolute path of its `kizz.md`
* `task.line`: the line of the block's opening fence, counting from 1
* `task.name`: the name that its `name=` option gives, or `None`
* `task.heading`: the nearest heading above the block, or `None`
* `task.targets`: the targets on the block
* `task.selected`: the requested targets that selected the task, and an empty list under `kizz run`
* `task.args`: the arguments of `kizz run`, as a list of strings, and an empty list under `kizz build`

## Running commands

`run("program", "argument", ...)` runs a program directly, without a shell; use `run("sh", "-c", "...")` for shell features.

* A program name without `/` is looked up in the `PATH` of the task environment, never in kizz's own.
  Without `PATH` in the task environment, a bare program name is an error that says to set or inherit `PATH`.
* Every location the lookup checks is a dependency of the command, so a program that appears earlier in `PATH` runs it again.
* The command runs in the task directory, or in `cwd`, relative to it.
* Its standard input is the `stdin` text, or `/dev/null`.
* Its output is always captured: `capture = "split"` keeps standard output and standard error apart,
  and `capture = "merged"` keeps them in one stream in their exact order.
  `stream = True` also shows the output live.
* The result has `rc`, the exit code or `None` when a signal killed the command, and `signal`.
  It has `stdout` and `stderr` with `split`, `output` with `merged`, and `cached`, whether it came from the cache.
  Output is decoded as UTF-8, with invalid bytes replaced; the cache keeps the raw bytes.

Output that a program captures and passes to a later command, as arguments, `stdin`, variables or a file,
becomes a dependency of that later command, so it runs again exactly when that output changes.

A positional argument can also be a list or tuple of strings, lists or tuples, which expands in place, at any depth,
so `run("echo", ["this", "is", "test"])` runs the same command as `run(["echo", "this"], "is", "test")`.
An empty list adds nothing, and each string stays one argument, never split on spaces.
So a program can build a command by logic rather than write it out literally,
and an optional argument can be a variable that holds `[]` when it is not wanted:

```starlark
env.inherit("PATH", "HOME")
verbose = ["--verbose"] if "verbose" in task.args else []
run("cargo", "test", verbose, "--locked", expect=[0])
```

`run("cargo", "test", task.args)` passes the arguments of `kizz run` through, each as one argument.
The [`argv` entry](@/reference/starlark.md#argv) of the reference gives every rule and error of the expansion.

## The task environment

Every task starts with an empty environment.
Nothing reaches a command unless the task puts it there, so each task is explicit about every variable its commands see.
The exact environment of each command is part of its cache key.

* `env.inherit("PATH", "HOME")` copies variables from the caller environment, the one kizz started with; unset ones are skipped.
* `env.caller(name, default = None)` reads a caller variable without changing the task environment.
* `env.set(name, value)`, `env.update(values)`, `env.unset(name)`,
  `env.get(name, default = None)` and `env.dict()` work on the task environment, and `env.clear()` empties it.
* `env.load_file(path, defaults = {}, required = True)` reads a dotenv file, fills in `defaults` for variables it does not set,
  adds the result to the task environment and returns it as a dict.
  The file is a dependency of the task, and with `required = False` a missing file means that only the defaults apply.
* `env.read(path)` reads a dotenv file into a dict without changing the task environment.
* `run(..., env = {...})` adds variables for one command only, and a `None` value removes one for that command.

Dotenv files use `NAME=value` lines, `#` comments, an optional `export` prefix, and single or double quotes.
Double quotes support `\n`, `\t`, `\"` and `\\`.
There is no `${VAR}` expansion, because expansion would silently pull values from elsewhere.

## Files and JSON

Task programs read and write files directly, so a build can adapt to its own data.
Paths are relative to the task directory, and absolute paths and paths outside the tree are allowed.

* `fs.read(path)` returns text, and `fs.write(path, text)` writes text, creating parent directories.
* `fs.read_json(path)` returns a Starlark value,
  and `fs.write_json(path, value, indent = 2)` writes stable JSON with a trailing newline.
* `fs.exists(path)`, `fs.list(path = ".")` and `fs.glob(pattern)` inspect the file system, with sorted results.

A write happens only when the content differs, so an unchanged file keeps its timestamps.
Kizz records every file access of the program, missing files and directory listings included,
and uses those records for the order of tasks and for explanations.
The program's own reads always see current data, because the program runs in every build.

## The order of execution and passes

Within a task, commands run in program order.
Tasks run one at a time.
Planned: `kizz build --parallel`, to run independent tasks at once; today it is an error ([planned features](@/guide/planned.md)).

* By default, parent directories come before their subdirectories, sibling directories come in sorted order,
  and the blocks of one file come in file order.
* After each build, kizz records which tasks consumed paths that another task wrote,
  through its commands or through `fs.write` and `fs.write_json`.
  The next build runs those producers before their consumers, and keeps the default order elsewhere.
* Every pass tries every selected task, and a failed task does not stop the pass.
* When a command ran in a pass, or a task program changed a file, kizz checks every selected task again in another pass.
  A task whose recorded inputs changed runs again; one whose inputs did not change is a cache hit.
* The build ends with a pass that runs nothing.
  The summary counts the passes that the build needed to settle, and not that final pass, which only confirms it.
* A build that has not settled after one more pass than it has tasks stops with an error that names the paths that keep changing
  and who writes them.

So a first build may need extra passes when tasks are in the wrong order, and later builds usually settle in one.
In this file, "Package" reads the file that "Generate" writes, but comes first:

````markdown
# Order

## Package

```starlark ci
env.inherit("PATH")
copy = run("cat", "gen/notes.txt", expect = [0])
fs.write("out/notes.txt", copy.stdout)
```

## Generate

```starlark ci
env.inherit("PATH")
run("sh", "-c", "mkdir -p gen; echo hi > gen/notes.txt", expect = [0])
```
````

The first build fails "Package", runs "Generate", and then runs "Package" again, because the file it looked for appeared:

```text
ran `cat gen/notes.txt` in kizz.md:5 (Package) step 1: never run: there is no record for this step
error: kizz.md:5 (Package) failed:
  command `cat gen/notes.txt`: exit code 1, but `expect` accepts exit code 0
  --- stdout ---
  --- stderr ---
  cat: gen/notes.txt: No such file or directory
  Traceback (most recent call last):
    * kizz.md:7, in <module>
        copy = run("cat", "gen/notes.txt", expect = [0])
  error: run(): command `cat gen/notes.txt`: exit code 1, which `expect` does not accept
   --> kizz.md:7:8
    |
  7 | copy = run("cat", "gen/notes.txt", expect = [0])
    |        ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
    |
ran `sh -c mkdir -p gen; echo hi > gen/notes.txt` in kizz.md:13 (Generate) step 1: never run: there is no record for this step
pass 2: a command ran in pass 1, so every selected task is checked again
ran `cat gen/notes.txt` in kizz.md:5 (Package) step 1: `gen/notes.txt` changed (appeared)
pass 3: a command ran and a task program changed a file in pass 2, so every selected task is checked again
build succeeded: 2 tasks, settled in 2 passes; 3 commands ran, 0 cached
```

The build succeeded, because no task failed in the pass that ended it.
The next build finds everything cached:

```text
build succeeded: 2 tasks, settled in 1 pass; 0 commands ran, 2 cached
```

## Failure handling

A non-zero exit code never fails a task by itself; the task program decides.

* With `expect = None`, the default, `run()` always returns, and the program inspects `rc`.
* With `expect = [0, 1]`, the task fails when the exit code is not in the list or a signal killed the command.
* `fail("message")` fails the task, and so does any error of the program.
* A command whose program cannot be found or started fails its task.

When a task fails, kizz prints the error, the Starlark stack, and the command with its exit status and captured output.

A non-zero exit or a signal is *unchecked* when its `run()` has no `expect` and the program never reads its `rc` or `signal`.
Reading them as attributes or with `getattr` checks a result, and so does encoding it with `json.encode` or `fs.write_json`;
reading only the output or `cached` does not.
Once the build settles, kizz warns loudly about each unchecked command of the task runs that stand,
which a later pass does not replace, naming the task, the command and how it ended, and repeats the warning in the summary.
`kizz build --fail-unchecked` makes each one fail its task instead, which suits CI.

A build has no fail-fast and no `--keep-going`: it tries every task, even after a failure.

* Kizz cannot tell why a task failed.
  When a path that a failed task looked for and did not find appears, the task runs again, as "Package" did above.
* A failed task whose missing paths are all still missing, and whose other inputs are unchanged,
  fails again from the cache without running.
* A bare program name without `PATH` and an invalid `ignore` or `track` pattern fail their task before kizz starts anything.
  Kizz checks both before the cache check, so every pass fails such a call the same way, and it counts as no command that ran.
* A tracer failure counts as a command that ran and leaves no record, and no later pass of the same build runs it again.
* When a pass runs nothing and failures remain, the build stops with exit status 1.
  It lists each failed task with the paths it looked for and did not find, since any of them may be the cause.
* Commands that finished before a failure keep their cache entries.

`kizz build` exits with 0 on success, 1 when a task fails, 2 for usage and configuration errors, and 130 when interrupted.
On Ctrl-C, kizz starts nothing new, the running command receives the interrupt, and its result is not cached.
Kizz kills every process of the command that still runs 2 seconds after the interrupt, then releases the cache and exits.

[starlark-spec]: https://github.com/bazelbuild/starlark/blob/master/spec.md "The Starlark language specification"
[starlark-dialect]: https://docs.rs/starlark/0.14.2/starlark/syntax/struct.Dialect.html "Dialect, in the starlark crate"
