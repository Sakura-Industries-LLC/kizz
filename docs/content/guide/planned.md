+++
title = "Planned features"
weight = 8
+++

This page lists the features that are planned but not built yet, and says for each what exists today.
Every page of this site marks a planned feature where it names one.

## kizz graph

Planned: `kizz graph` will show the learned task graph, the order in which tasks consume what other tasks write.
Today the command is parsed but not implemented: it prints that it is not wired to behavior yet and exits with status 2.
Kizz already learns and uses the order in every build, and `kizz list` lists the tasks.

## kizz image

Planned: `kizz image` will build container images, reusing each layer while what its steps read has not changed.
Today the command is parsed but not implemented: it prints that it is not wired to behavior yet and exits with status 2.

## kizz build --parallel

Planned: `kizz build --parallel` will run independent tasks at the same time, scheduled from the learned order.
Today `--parallel` is an error that exits with status 2, and `--sequential`, the default, runs one task at a time.

## Output restoration

Planned: kizz will restore the files that a cached command wrote from the cache, instead of running the command again.
Restoration will also let a project that moved to another path reuse its cache entries.
Today a cache hit replays a command's exit status and captured output, but not the files it wrote.
A command whose output files changed or disappeared runs again, and a moved project is built again.

## The aarch64 tracer

Planned: a tracer for Linux on aarch64.
Today kizz traces commands only on Linux on x86_64.

## graph.json

Planned: a `graph.json` file, kept with the project, will hold the learned task order.
Then kizz in CI, possibly without a cache, will run tasks in the right order from the start.
Today the learned order lives only in the cache, so a build without one may take extra passes,
in which a task can fail until the task that writes what it needs has run.

## A warning for kizz.md files that Git might ignore

Planned: a loud warning for each `kizz.md` that Git might ignore, which a fresh checkout would lack.
Today discovery uses every `kizz.md` that it finds, including those that Git ignores, without a warning.

## Interactive commands in kizz run

Planned: `kizz run` will be able to give a command the terminal, so that it can read from it interactively.
Today a command's standard input under `kizz run` is the `stdin` text of its `run()`, or `/dev/null`, as in a build.
`kizz execute` already gives its program the terminal.
