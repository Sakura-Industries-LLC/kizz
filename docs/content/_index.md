+++
title = "kizz"
weight = 0
+++

Kizz is a build engine for Linux.
It runs small Starlark programs found in `kizz.md` files, traces every command those programs run,
and records what each command touched as that command's dependencies, so nobody writes dependency lists by hand.

## A first example

A task is a fenced `starlark` block in a `kizz.md` file, tagged with the targets that select it:

````markdown
## Build the library

```starlark ci
env.inherit("PATH", "HOME")
build = run("cargo", "build", "--locked", stream = True)
if build.rc != 0:
    fail("cargo build failed")
```
````

Running `kizz build ci` there runs the task, and a later run skips the command while its recorded dependencies are unchanged:

```sh
kizz build ci
```

## Where to read on

* The [guide](@/guide/_index.md) explains what kizz is, how to start, and how to use it.
* The [reference](@/reference/_index.md) describes the command line, the environment, the reports, `kizz.md` and Starlark.
* The [project](@/project/_index.md) section holds reports about kizz itself, such as its measurements.
