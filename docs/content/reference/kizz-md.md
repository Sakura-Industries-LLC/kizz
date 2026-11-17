+++
title = "The kizz.md format"
weight = 4
+++

This page describes the format of a `kizz.md` file: its task blocks, their targets, and their task options.
[The kizz.md guide](@/guide/kizz-md.md) explains how to write one,
and the [Starlark API](@/reference/starlark.md) describes what a task program can call.

## Overview

A `kizz.md` is an ordinary Markdown file, and kizz reads only its task blocks.
A task block is a fenced code block whose info string starts with the word `starlark`.
The rest of the info string tags the block with targets and task options, and the block's content is the task's program:

````markdown
# Server

## Lint the sources

```starlark ci check name=lint
env.inherit("PATH", "HOME")
run("cargo", "clippy", "--", "-D", "warnings", expect = [0])
```
````

Here `ci` and `check` are targets, `name=lint` is a task option, and "Lint the sources" is the task's heading.
`kizz build ci`, `kizz build check` and `kizz run lint` all run this task.

Everything else in the file is documentation, so a `kizz.md` can explain its tasks in prose, lists and other code blocks.

## File name and discovery

Kizz reads only files named exactly `kizz.md`; every other Markdown file is documentation, which kizz does not read.

* `kizz build`, `kizz run` and `kizz list` start in the current directory, and do not look above it.
* They find every `kizz.md` in that directory and every directory below it, hidden directories included.
* Kizz does not consult Git or any ignore file: every `kizz.md` it finds is used, even one that Git ignores.
  Planned: a warning for each `kizz.md` that Git might ignore ([planned features](@/guide/planned.md)).
* Symbolic links to directories are followed.
  A link that would make the walk recurse into one of its own parent directories is skipped with a warning.
* A directory or file that kizz cannot read is skipped with a warning, so a `kizz.md` behind it is not found.
* A `kizz.md` reached through several paths is used once, under the first of those paths in the default order.
* A `kizz.md` that cannot be read, or that is not valid UTF-8, is a [configuration error](#configuration-errors).

The default order of tasks is the order of discovery: a directory's `kizz.md` before those of its subdirectories,
sibling directories in the byte order of their names, and the blocks of one file in file order.
A build changes it only where it has learned that a task consumes what another task writes,
as [the kizz.md guide](@/guide/kizz-md.md#the-order-of-execution-and-passes) explains.

## Fences

Kizz reads Markdown as [CommonMark][commonmark] does, so a task block is a fenced code block by CommonMark's rules:

* The fence is three or more backticks, or three or more tildes, and the closing fence uses the same character,
  at least as many times.
* A block inside a list item or a block quote counts too.
* An indented code block, without a fence, is not a task block.
* A `starlark` block nested inside another fenced block, such as a `markdown` example with a longer fence, is that block's content,
  not a task block.
  This page shows its examples that way.
* A block that the file never closes ends with the file, as in CommonMark.

The block's content, the lines between its fences, is the task's Starlark source.
Its line numbers are kept: a Starlark error in the block names the line of the `kizz.md`.

## The info string

The info string is the text after the opening fence.
Kizz splits it into words at whitespace:

* The first word must be exactly `starlark`.
  `Starlark`, `starlark,ci`, `python` and an empty info string make an ordinary code block, which kizz ignores.
* After `starlark`, a word that holds `=` is a [task option](#task-options), and every other word is a [target](#targets).
* Targets and options may come in any order, such as `starlark name=lint ci` or `starlark ci name=lint check`.
* A block needs at least one target or a name to be a task; otherwise it is an [untagged block](#untagged-blocks).

## Targets

A target is the name of a group of tasks that people build together, such as `ci`, `check` or `test`.
A task can have any number of targets, and many tasks in many `kizz.md` files can share one.

* A target name is one or more ASCII letters, digits, `_`, `.` and `-`.
  Any other character in a target word is a [configuration error](#configuration-errors).
* Names are compared as written, so `CI` and `ci` are different targets.
* A target repeated on one block counts once; [`task.targets`](@/reference/starlark.md#task-targets) lists the targets
  in the order written, without repeats.
* Targets are not part of the task program, so changing them changes no step key.

The command line selects tasks by a *target expression*: one target, or several joined with `+`, such as `ci+check`.

* `kizz build ci+check` runs every task that has `ci` or `check`, and a task with both runs once.
  [`task.selected`](@/reference/starlark.md#task-selected) holds the requested targets that selected it.
* `kizz list ci+check` lists the same tasks; `kizz list` without a target lists every task.
* `kizz build` without a target is an error, with exit status 2, that lists the targets that exist:

```text
error: no target given; give one target, or several joined with `+`; the available targets are: check, ci
```

* An empty part or an invalid name in the expression is an error, with exit status 2:

```text
error: invalid target expression `ci+`: a target is missing; join targets with `+`, such as `ci+check`
```

* A target that selects no task is not an error: `kizz build` warns about it and lists the targets that exist.
* `kizz run` selects a task by its name, not by its targets.

## Task options

A task option is a `key=value` word of the info string, after `starlark`.
There are two, [`name=`](#name) and [`cache=never`](@/reference/kizz-md.md#cache-never).

* A block gives each key at most once.
* Options are checked on every `starlark` block, an untagged one included, so a mistake in one is always a configuration error.
* Options are not part of the task program, so changing them changes no step key.
* `kizz list` shows each task's options after its targets, in the order `name=`, then `cache=`:

```text
kizz.md:5 (Lint the sources) [ci, check] name=lint
kizz.md:12 (Deploy) [] name=deploy cache=never
```

These are the problems of a task option, each a [configuration error](#configuration-errors) with its own message:

* a word that starts with `=`, which has no key:
  "task option `=x` has no key: a task option is a `key=value` word";
* a key other than `name` and `cache`:
  "unknown task option `color=red`: the task options are `name=<name>` and `cache=never`";
* a key that the block already gave: "task option `name=b` gives the key `name` again, which a block gives once";
* nothing after `=`: "task option `name=` has an empty value".

### `name=`

`name=<name>` names the task, so that [`kizz run <name>`](@/reference/command-line.md#kizz-run) can run it.

* Values: a name of one or more ASCII letters, digits, `_`, `.` and `-`, the characters of a target name.
  It has no default: a task without the option has no name.
* A name is unique within its `kizz.md`.
  Tasks in different `kizz.md` files may share a name, and `kizz run` picks the one nearest below the current directory,
  as [running one task](@/guide/run.md#how-the-name-is-found) explains.
* A task with a name and no targets is a task that only `kizz run` selects; `kizz build` never selects it,
  because a build selects by targets.
* The program reads the name as [`task.name`](@/reference/starlark.md#task-name).
* The task's `print()` lines start with its name, which takes the place of its heading there.
* Tracks: nothing; the name is not part of the task program or of any step key.
* Side effects: none.
* Errors:
    * an invalid name: "invalid task name in `name=a/b`: task names use only letters, digits, `_`, `.` and `-`";
    * an empty value: "task option `name=` has an empty value";
    * a name that an earlier block of the same file already has, which names that block's line:
      "task option `name=lint` gives a name that the task at line 5 of this file already has,
      and a name is unique within its kizz.md".

### `cache=never`

`cache=never` makes every command of the task run in every build that reaches it,
as if each of its `run()` calls had `always = True`.

* Values: `never`, the only value that `cache` accepts.
  Without the option, the task's commands are cached as their `run()` calls say.
* Each command of the task runs at most once per build, when a pass first reaches it, without a cache check,
  and later passes of the same build return that run's result again.
  Its reason is "its task is marked `cache=never`" when it has a record:

```text
run `echo deploying production` (step 1): its task is marked `cache=never`
```

* Under `kizz run`, every command of the task runs.
* No option makes such a command a cache hit: there is no option that forces caching.
* Use it for a task whose effect must happen every time it is asked for, such as a deployment,
  and give such a task a name and no target, so that no build runs it by accident.
* Tracks: everything that the task's commands touch is still recorded, as for every step.
* Side effects: those of the task's commands, in every build or run that selects the task.
* Errors: any other value, such as "invalid value in task option `cache=sometimes`: `cache` accepts only `never`",
  and an empty value.

## The heading

The task's heading is the nearest Markdown heading above its block.

* Its text is the heading's plain text, without its Markdown markup: `` ### `fmt` *check* `` gives `fmt check`.
  Line breaks in a heading become spaces, and the text is trimmed.
* Both `#` headings and underlined headings count, and so does a heading inside a list item or a block quote.
* A task above every heading has none, and so does a task whose nearest heading has no text.
* The program reads it as [`task.heading`](@/reference/starlark.md#task-heading).
* Output names a task by its file, line and heading, as `path/kizz.md:LINE (Heading)`, or `path/kizz.md:LINE` without a heading.
  The path is relative to the current directory, and the line is the line of the opening fence, counting from 1.
* `print()` prefixes each line with the task's name, else its heading, else `path/kizz.md:LINE`.
* The heading is not part of the task program or of any step key.

A short heading that says what the task does makes kizz's output easy to read.

## Task identity

A task is identified by the absolute path of its `kizz.md`, the path through which discovery reached it,
and its position among the task blocks of that file, counting from zero.

* Untagged blocks take no position.
* The cache keeps each task's program hash, the step keys and reasons of its steps by position, its learned order,
  and its ownership of step records under that identity.
  `kizz why` and `kizz cache reset` find a task's entries by it.
* Inserting or removing a task block before a task gives that task another identity.
* Step keys do not contain the identity: a block whose source and directory are unchanged keeps its step keys,
  wherever it moves in its file.
* Two tasks are not confused even when they have the same heading or name in different files.

## Untagged blocks

A `starlark` block with neither targets nor a name is not a task, because nothing could select it.
Kizz warns about it, because it is probably a mistake, and repeats the warning in the summary:

```text
warning: kizz.md:3 (Notes): this starlark block has neither targets nor a name, so it is not a task
```

* Its content is not run.
* Its task options are still checked, so a block with only `cache=never` warns, and one with `cache=sometimes` is a
  configuration error.
* It takes no position among the tasks of its file.

## Configuration errors

A configuration error anywhere under the current directory stops `kizz build`, `kizz run` and `kizz list` before any task runs,
with exit status 2.
Kizz prints every configuration error of every file, each with the file, the line of the block's opening fence and its heading.
For a file whose blocks have several mistakes, the output starts:

```text
error: kizz.md:5 (One): invalid value in task option `cache=sometimes`: `cache` accepts only `never`
error: kizz.md:10 (Two): invalid target name `ci!`: target names use only letters, digits, `_`, `.` and `-`
error: kizz.md:10 (Two): task option `=x` has no key: a task option is a `key=value` word
error: kizz.md:10 (Two): unknown task option `color=red`: the task options are `name=<name>` and `cache=never`
error: kizz.md:10 (Two): task option `name=b` gives the key `name` again, which a block gives once
```

The configuration errors are:

* a `kizz.md` that cannot be read, or that is not valid UTF-8: "could not read path/kizz.md: ...";
* an invalid target name;
* each problem of a [task option](#task-options),
  and the errors of [`name=`](#name) and [`cache=never`](@/reference/kizz-md.md#cache-never);
* more task blocks in one file than a 32-bit position can number: "path/kizz.md: more task blocks than kizz can number".

A block with a configuration error is not a task.
A Starlark error in a block is not a configuration error: it fails only that task, when it runs.

## Caching controls

This list gathers everything that decides whether a command runs or comes from the cache.
By default, kizz records what each command touched, and replays the command's recorded result while none of it has changed.

In a `run()` call:

* [`always = True`](@/reference/starlark.md#always) runs the command once in each build that reaches it, without a cache check.
* [`timestamps = True`](@/reference/starlark.md#timestamps) makes a new modification or change time of a used path a change.
* [`machine = [...]`](@/reference/starlark.md#machine) makes named parts of the machine state inputs of the step.
* [`track = [...]`](@/reference/starlark.md#track) makes the content of `/proc` and `/sys` files inputs of the step.
* [`ignore = [...]`](@/reference/starlark.md#ignore) leaves paths out of the step's record, so they never run it again.
* The program and its arguments ([`argv`](@/reference/starlark.md#argv)), [`stdin`](@/reference/starlark.md#stdin),
  the command's environment ([`env`](@/reference/starlark.md#env)), the working directory
  ([`cwd`](@/reference/starlark.md#cwd)) and [`capture`](@/reference/starlark.md#capture) are part of the step key,
  so a change to any of them gives another step key, and the record under that key decides:
  a key without a record runs, and a key that an earlier value left a valid record under, such as after an edit is undone,
  is a cache hit again.
* [`stream`](@/reference/starlark.md#stream) and [`expect`](@/reference/starlark.md#expect) are not part of the step key
  and change no caching.
* The task program's hash, which covers the block and every module that it [loads](@/reference/starlark.md#load),
  is part of every step key of the task, so editing either gives the task's commands other step keys,
  and each runs again unless a record under its new key is still valid.

In the info string:

* [`cache=never`](@/reference/kizz-md.md#cache-never) runs every command of the task as if it had `always = True`.
* Targets, the name and the heading change no step key.

On the command line:

* [`kizz run --no-cache`](@/reference/command-line.md#no-cache) runs every command of its task, and still records each result.
* [`--cache-unknown-syscalls`](@/reference/command-line.md#cache-unknown-syscalls) of `kizz build` and `kizz run`
  caches the results of commands that made a system call that the tracer does not know, which are not cached by default.
* [`kizz cache reset`](@/reference/command-line.md#kizz-cache-reset) wipes the entries of the tasks under a path,
  so their commands run again, and [`kizz cache clean`](@/reference/command-line.md#kizz-cache-clean) removes the entries
  about files that no longer exist.

No option forces caching: a command marked `always`, or of a task marked `cache=never`, runs whatever else is given.
Some results are never a cache hit in a later build, such as that of a command killed by a signal.
While no path that such a command touched has changed, a later pass of the same build does not run it again:
it returns the same result, with `cached` set, and a command that could not start, which has no result,
fails its task again with the same error.
[The caching guide](@/guide/caching.md#results-that-are-not-cached) lists them.

## Recommended usage

[Recommended usage](@/guide/recommended-usage.md) gives the advice in full; in short:

* Use a few well-known targets, such as `ci`, `check` and `test`, and give a task every target whose group needs it.
* Put a short heading above each task, because output names tasks by their headings.
* Name the tasks that people run by hand, and give a task that must run every time `name=` and `cache=never` without a target.
* Inherit only the variables that a task's commands need, with `env.inherit()`.
* Give `expect` to every command whose failure must fail the task, and use `kizz build --fail-unchecked` in CI.
* Share common steps in Starlark modules with `load()`.

[commonmark]: https://spec.commonmark.org/current/#fenced-code-blocks "Fenced code blocks, in the CommonMark specification"
