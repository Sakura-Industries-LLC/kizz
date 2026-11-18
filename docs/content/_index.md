+++
title = "kizz"
weight = 0
+++

Kizz is a build engine for Linux.
It runs small [Starlark][starlark-spec] programs that it finds in `kizz.md` files.
It traces every command those programs run, and records what each command touched as that command's dependencies.
So nobody writes dependency lists by hand: kizz observes them, and runs a command again only when it has a reason to.

## A first example

A task is a fenced `starlark` block in a `kizz.md` file.
The words after `starlark` are the targets that select it, such as `ci`, and options such as `name=`:

````markdown
# Hello

## Count the words

```starlark ci
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

`kizz build ci` runs every task tagged `ci` in the current directory and below.
The first build runs `wc`; the second finds that nothing `wc` touched has changed, and replays its recorded result:

```sh
kizz build ci
kizz build ci
```

`kizz run greet` runs the one task named `greet`, and the words after `--` reach its program as `task.args`:

```sh
kizz run greet -- kizz
```

`kizz execute` runs any command, without a `kizz.md` and without the cache, and writes a report of what it touched:

```sh
kizz execute --output=report.txt -- sh -c 'cat notes.txt | wc -w'
```

## Where to read on

* The [guide](@/guide/_index.md) explains [what kizz is](@/guide/what-is-kizz.md), how to [get started](@/guide/getting-started.md),
  how to write [`kizz.md` files](@/guide/kizz-md.md), and how [caching](@/guide/caching.md),
  [`kizz execute`](@/guide/execute.md) and [`kizz run`](@/guide/run.md) work.
* The [reference](@/reference/_index.md) describes the [command line](@/reference/command-line.md),
  the [environment variables](@/reference/environment.md), the [report format](@/reference/execute-report.md),
  the [`kizz.md` format](@/reference/kizz-md.md) and the [Starlark functions](@/reference/starlark.md) of kizz.
* The [project](@/project/_index.md) section holds reports about kizz itself, such as its [measurements](@/project/measurements.md).
* [Planned features](@/guide/planned.md) lists what is planned but not built yet.

[starlark-spec]: https://github.com/bazelbuild/starlark/blob/master/spec.md "The Starlark language specification"
