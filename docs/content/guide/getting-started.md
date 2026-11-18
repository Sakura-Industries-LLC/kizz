+++
title = "Getting started"
weight = 2
+++

This page takes you from building kizz to a first `kizz.md`, a first build, and first uses of `kizz execute` and `kizz run`.

## What kizz needs

* Linux 5.4 or later, on x86_64.
  Kizz needs Linux 5.4 or later because its tracer collects the first process of each command by a process descriptor:
  it opens the descriptor with `pidfd_open` and collects the process with `waitid` and `P_PIDFD`, which Linux 5.4 added.
  There is no fallback for older kernels.
* Permission to trace: kizz traces commands with `ptrace` and installs a seccomp-bpf filter in each process it starts.
  Where `ptrace` or seccomp is refused, for example by the security settings of a container, kizz cannot run commands.
* No other operating system or processor: the tracer is for x86_64 Linux.
  Planned: an aarch64 tracer, which [planned features](@/guide/planned.md) describes.

Each command that kizz starts also runs with the `no_new_privs` flag of Linux,
so a set-user-ID program such as `sudo` does not gain privileges under kizz.

## Building kizz

Kizz is written in Rust and needs Rust 1.100 or later.
Build it with Cargo from a checkout of its repository:

```sh
cargo build --release
```

The binary is `target/release/kizz`; put it, or a link to it, in a directory on your `PATH`.
`kizz --help` lists the commands, and `kizz <command> --help` describes each one.

Every command except `kizz execute` prints a banner first, unless `--quiet` or `--no-banner` is given.
The examples in this guide pass `--no-banner`, so their output shows only what the command itself prints.

## A first kizz.md

Make a directory with a file named `notes.txt` that holds `one two three`, and a file named `kizz.md`:

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

The file holds two tasks:

* "Count the words" has the target `ci`, so `kizz build ci` selects it.
  It copies `PATH` from your environment into its own, which starts empty, runs `wc`, and writes what `wc` printed to a file.
  `expect = [0]` fails the task if `wc` exits with anything but 0.
* "Greet" has no target and the name `greet`, so only `kizz run greet` runs it.
  It greets its first argument, or the world.

The [kizz.md page](@/guide/kizz-md.md) explains every part of the file.

## Listing the tasks

`kizz list` shows every task under the current directory, with its file, line, heading, targets and options:

```sh
kizz --no-banner list
```

```text
kizz.md:5 (Count the words) [ci]
kizz.md:13 (Greet) [] name=greet
2 tasks in 1 kizz.md file.
```

## A first build

`kizz build ci` runs every task tagged `ci`, in the current directory and below:

```sh
kizz --no-banner build ci
```

```text
ran `wc -w notes.txt` in kizz.md:5 (Count the words) step 1: never run: there is no record for this step
pass 2: a command ran and a task program changed a file in pass 1, so every selected task is checked again
build succeeded: 1 task, settled in 1 pass; 1 command ran, 0 cached
```

Kizz printed each command it ran, with the reason it ran.
Because a command ran, kizz checked the task again in a second pass, in which nothing ran, so the build was finished.
[The kizz.md page](@/guide/kizz-md.md) explains passes.

Build again, and the command is cached:

```text
build succeeded: 1 task, settled in 1 pass; 0 commands ran, 1 cached
```

The task program itself ran again, as it does in every build, but `run()` gave it the recorded result of `wc`.

Now change `notes.txt` to `one two three four` and build again:

```text
ran `wc -w notes.txt` in kizz.md:5 (Count the words) step 1: `notes.txt` changed (size changed from 14 to 19 bytes)
pass 2: a command ran and a task program changed a file in pass 1, so every selected task is checked again
build succeeded: 1 task, settled in 1 pass; 1 command ran, 0 cached
```

## Asking why

`kizz why` prints the reasons that the most recent build recorded:

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

`kizz build --explain` prints the full reasons as the build runs, for cached commands too.
[Caching](@/guide/caching.md) lists every reason.

## Where the cache lives

One cache serves every build on the machine, wherever it starts.
Kizz finds it in this order:

1. `KIZZ_CACHE_DIR`, when it is set and not empty; it must be an absolute path.
2. `$XDG_CACHE_HOME/kizz`, when `XDG_CACHE_HOME` is an absolute path.
3. `~/.cache/kizz`, from an absolute `HOME`.

The cache directory holds the database, in `db`, and the lock file `kizz.lock` while a command uses the cache.
Only one command uses a cache at a time: another one waits, and says which command it is waiting for.
The cache belongs to the machine and the user that created it: kizz refuses to use it when its recorded host name, credentials,
umask, architecture or CPU model differ from the current ones.
`kizz cache clean` and `kizz cache reset` clean it up, as [caching](@/guide/caching.md) explains.

## A first kizz execute

`kizz execute` runs any command and writes a report of every path that it touched, process by process.
It needs no `kizz.md`, and it never uses the cache.
Here it runs a pipeline through `sh` in the same directory:

```sh
kizz execute --output=report.txt -- sh -c 'cat notes.txt | wc -w'
```

The command's output, `4`, goes to the terminal as usual, and `report.txt` holds the report.
[Observing a command](@/guide/execute.md) shows the report and explains how to read it.

## A first kizz run

`kizz run` runs one task by its name, and shows each command and its output as it runs:

```sh
kizz --no-banner run greet
```

```text
run `echo hello, world` (step 1): never run: there is no record for this step
hello, world
run succeeded: kizz.md:13 (Greet); 1 command ran, 0 cached
```

The words after `--` are the task's arguments:

```sh
kizz --no-banner run greet -- kizz
```

```text
run `echo hello, kizz` (step 1): the arguments changed: word 3: `world` became `kizz`
hello, kizz
run succeeded: kizz.md:13 (Greet); 1 command ran, 0 cached
```

[Running one task](@/guide/run.md) explains `kizz run` in full.
