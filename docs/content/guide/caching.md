+++
title = "Caching"
weight = 4
+++

This page explains how kizz decides that a command can be skipped, and the options that change what it tracks.

## Overview

Kizz records what each command touched, and normally skips a command whose recorded inputs have not changed.
A command runs anyway when kizz has no record for it, or when something it recorded has changed.
It also runs in every build that reaches it when its `run()` has `always = True`, when its task is marked `cache=never`,
or under `kizz run --no-cache`.
And a command whose previous result could not be cached, such as one that a signal killed, runs again.
Every time a command runs, kizz says why.

## What kizz records

A *step* is one command that a task runs with `run()`.
For each step, kizz records every path that the command and its whole process tree touched,
apart from those that the step's `ignore` patterns match, with how it touched it:

* read: content was read
* probe: existence or metadata was checked, including lookups that found nothing
* list: a directory's entries were read
* write: the path was created, modified, renamed, removed or had its metadata changed
* exec: the path was run as a program, or the kernel loaded it to run one, as a `#!` interpreter or an ELF loader
* xattr: the path's extended attributes were read or listed

A lookup that finds nothing is a dependency: when the path appears, the step runs again.
A directory listing is a dependency too, including the order in which the file system returns the entries.

When the command finishes, kizz records the state of every path it touched:

* a regular file that was read or run: its size, times, inode, device, type, permissions, owner and a BLAKE3 hash of its content;
* any other regular file, such as one that was only checked or written: the same metadata, without a hash;
* a directory: its type, permissions, owner and times, and for a listed one, a hash of its entries in their order;
* a symbolic link: its target, and the state of what it points to;
* a device, FIFO or socket: its type only;
* a path under `/proc` or `/sys`: that it was used, which is never compared unless the step tracks it.

A path whose extended attributes were read or listed also records their names and a hash of each value.

The record also holds the command's exit status, its captured output, and the state of the machine when it started.

## Step keys

Each step's record is stored under a *step key*, a hash of everything that is known before the command runs:

* the task program's hash, covering the block and every module it loads;
* the absolute working directory, the arguments, the environment and the `stdin` text;
* the capture mode, the `ignore` and `track` patterns, whether `timestamps` is set, and the `machine` names;
* the occurrence number, which counts identical commands earlier in the same task run.

A change to any of these makes a new key, which has no record yet, so the step runs.
`always`, `stream` and `expect` are not in the key: they change what kizz does with a step, not what the command does.
Task options are not in it either.

Keys use the absolute directory of each task, not the directory where kizz started.
So building deep in a tree and then several directories higher reuses the results already built below.
Moving a project to a new path builds it again.
Planned: output restoration, which would let a moved project reuse its entries ([planned features](@/guide/planned.md)).

## The cache check

For a step with a record, kizz compares every recorded path with the file system, concurrently.
If everything matches, the step is a cache hit: `run()` returns the recorded exit status and output, and nothing runs.
If anything differs, the step runs, and its explanation lists every difference.

A regular file that was read is compared like this:

* A different type, permissions or owner is a change, whatever the content.
* A different size is a change, found without hashing.
* The same size and the same metadata mean no change.
* The same size with different times, inode or device means maybe changed, so kizz hashes the file.
  If the hash is the same, the file is unchanged, and kizz stores its new metadata so the next check is fast.
* A file modified within two seconds of when its record was written is always hashed,
  because its metadata cannot prove that it is unchanged.

A file that was not read has no hash, so any change of its size or metadata is a change.
A listed directory is listed again and compared by its entries, in order.
Tools that restore modification times, such as `tar`, `rsync -t` or `cp -p`, still change the change time,
so kizz notices the files they rewrite.

## What makes a command run again

Each of these runs a command, and its explanation says which one did:

* never run: there is no record for this step;
* the task program changed: its block, or a named loaded module;
* the arguments changed, word by word;
* the environment changed: which variables were added, removed or changed, with their values only under `--verbose`;
* the working directory, `stdin`, capture mode, ignore patterns, `track` patterns, `timestamps` or `machine` changed;
* inputs changed: every changed path, what changed about it, and who last wrote it;
* the machine state that the step names changed, with each old and new value;
* the command is marked `always`;
* its task is marked `cache=never`;
* `kizz run --no-cache` was given;
* the previous run could not be cached, and why.

## Results that are not cached

Some results are never a cache hit in a later build: their commands run again in the next build that reaches them.
The reason is that the previous run could not be cached, and why, except for a command marked `always`,
a task marked `cache=never` and a step under `kizz run --no-cache`, whose reason is that marking or option,
because kizz checks it first.
Within one build, a later pass runs such a command again only if a path that it touched changed:

* a command that a signal killed, including an interrupted one;
* a run that the tracer reports as incompletely traced, such as one that changed the root directory or mounted a file system;
* a run that made a system call the tracer does not know, unless `kizz build --cache-unknown-syscalls` is given;
* a run in which a file changed after the command first read it or ran it;
  the explanation names each such file and says whether the command itself wrote it;
* a command that could not start for a reason other than a missing or non-executable program.

Until such a path changes, a later pass of the build returns the same result, with `cached` set,
and a command that could not start fails its task again with the same error.
A run in which something other than the command changed a file it read gets a loud warning.
A command whose program is missing or not executable fails its task, and runs again once the program appears.
A tracer failure fails its task and leaves no record, and no later pass of the same build runs that command again.
The next build that reaches it decides as if that run had not happened: an older record of the step stays,
and is used again if its inputs match.

## Specifying caching

The user decides how each command is cached, with options of `run()`, a task option, and an option of `kizz run`.
The [Starlark reference](@/reference/starlark.md) lists every parameter.

### always

`run(..., always = True)` runs the command in every build that reaches it, for a command whose effect kizz cannot see,
such as an upload, a download or a notification.
It runs at most once per build, when a pass first reaches it, and later passes of that build reuse its result.
Its dependencies are still recorded, and if it rewrites files with the same content, the steps that read them stay cached.

```text
ran `sh -c echo uploading` in kizz.md:5 (Upload) step 1: marked `always`
```

### timestamps

`run(..., timestamps = True)` also counts a new modification or change time of a file or directory that the step read, ran,
listed or checked as a change, even when the content is the same.
It suits tools such as `tar` and `zip`, which store timestamps in what they write.
Access times, and the times of a symbolic link itself, are never compared.

### machine

The state of the machine and of the process that a command starts with can change what it does.
Kizz records it with every step, and `run(..., machine = [...])` names the parts that count as inputs of the step:

* `kernel`: the kernel release and version
* `limits`: every resource limit, soft and hard
* `affinity`: the CPUs the command may run on
* `memory`: the total and the available memory
* `filesystem`: the type and mount flags of the file system that holds the working directory
* `disk`: the size and free space of that file system

Available memory and free space change in almost every build, so a step that names `memory` or `disk` runs almost every time.
An unknown name fails the task.

### track

`run(..., track = [...])` lists `/proc` and `/sys` files whose content counts as an input of the step.
Kizz hashes a tracked file when it records the step and reads it again when it checks the cache.
The patterns take the same forms as `ignore`.

### ignore

`run(..., ignore = [...])` lists paths that are not recorded as dependencies of the step.
A pattern is a path, a glob, or a regular expression after `re:`, matched against absolute paths.
A relative glob is anchored at the task directory, and a leading `~/` means the caller's home directory.
Starlark variables make reusable sets, such as `GO_CACHE = ["~/.cache/go-build/**"]`.
`kizz why` reports how many accesses each step ignored, and kizz never adds an exception of its own.

### cache=never

The task option `cache=never` makes every command of the task run in every build that reaches it, as if each had `always = True`:

```text
run `true` (step 1): its task is marked `cache=never`
```

No option makes such a command a cache hit.

### kizz run --no-cache

`kizz run --no-cache` runs every command of its task, even one that a cache hit would replay, and still records each result:

```text
run `echo hello, world` (step 1): `kizz run --no-cache` was given
```

There is no option that forces caching: a command marked `always`, or of a task marked `cache=never`, always runs.

## /proc and /sys

The files under `/proc` and `/sys` are kernel interfaces, and many of them change on every read.
Kizz records every access to them, so `kizz why` shows that a command used them, but by default they never run a step again.
Track the ones that matter with `track`.
Per-process paths such as `/proc/self` describe kizz when it checks the cache, not the command, so tracking them is rarely useful.

## Behavior kizz cannot see

Kizz assumes that a command does nothing it cannot observe: no dependence on time, randomness, the network, process IDs,
or anything else that is not a file access.
It also assumes that a command has no effect outside the files it writes, so a cache hit does not repeat an upload.
Mark a command that breaks either assumption with `always = True`.

* Kizz detects connections to the network and to Unix sockets, because what comes back is an input it cannot see.
  It warns loudly about each connection when the command runs, naming the address or socket, and repeats it in the summary.
  The result is still cached, and `kizz why` lists the step's connections.
* A system call that the tracer does not know may have a meaning it cannot judge.
  Kizz warns about it by number, and the result is not cached unless `--cache-unknown-syscalls` is given.
* Devices, FIFOs and sockets are compared by type only, never by what they deliver.

## The cache identity

The cache is not portable: it belongs to the machine and the user that created it.
When kizz creates the database, it stamps it with the host name, the user's credentials, the umask,
the machine architecture and the CPU model.
If any of them differs later, kizz refuses to use the cache, names each value that differs, and exits with status 2.
`kizz cache reset --all` still wipes such a cache, and a different cache, set with `KIZZ_CACHE_DIR`, avoids the conflict.

## Cleaning up the cache

The cache only grows during builds.
These commands clean it up; they change cache entries only, never project files.

* `kizz cache clean` removes the entries about files and dependencies that existed when kizz recorded them but no longer exist.
  A step record that depends on such a file is removed as a whole.
  A dependency on a file that was already missing when kizz recorded it is kept, because it is still valid.
  It asks for no confirmation, because the worst case is a rebuild.
* `kizz cache reset [path]` wipes every entry for the tasks under the path, which defaults to the current directory.
* `kizz cache reset --all` wipes the whole cache.

Both resets show what they will remove and ask for confirmation first.
`--yes` skips the question, and without a terminal a reset refuses to run unless `--yes` is given.

```text
checked 7 recorded paths; 0 no longer exist
nothing to remove: no cache entry is about a file that no longer exists
```

## Explanations

Kizz says why it ran each command, so nobody has to guess why something keeps rebuilding.

* `kizz build` prints a one-line reason for each command it runs, and `--explain` prints the full reasons,
  for cached commands too.
* `--verbose` also lists the cached commands, and shows the values of changed environment variables.
* `kizz why [path]` prints the reasons that the most recent build stored, for the tasks under the path.
  It also lists each step's untracked `/proc` and `/sys` accesses, its connections, its ignored accesses,
  and the system calls it made that the tracer does not know.

When a step runs again because of a file that it, or another step, writes on every build, the explanation says so.
A step that runs again in consecutive builds for the same self-inflicted reason gets a loud warning,
so that you can decide whether an `ignore` pattern belongs there.
