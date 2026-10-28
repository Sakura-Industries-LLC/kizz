# Kizz architecture and version 1 plan

License: AGPL-3.0-only.
Status: working plan for the first version, revised on 2026-10-23 from the original outline.

This plan turns the original outline into a design for the first version of kizz.
It records every decision made so far and fills in the remaining design.
Anything marked *Proposed* has not been decided yet;
[Open questions](#open-questions) lists each of those items with the phase that needs the answer.

## Glossary

* `kizz.md`: a Markdown file named exactly `kizz.md`.
  These are the only files kizz reads to find work.
* Target: a name given on the command line, such as `ci` or `check`.
* Task: a Starlark code block in a `kizz.md`, tagged with one or more targets, a name, or both.
  Each task is a small program.
* Task option: a `key=value` word in a task block's info string, such as `name=lint` or `cache=never`.
* Step: one command that a task runs with `run()`.
  Steps are what kizz traces and caches.
* Observation: one path a step touched, how it touched it, and the path's state when the step finished.
* Caller environment: the environment kizz itself was started with.
* Task environment: the environment a task builds explicitly and passes to the commands it runs.

## Decision record

These decisions came out of the review of the original outline.

| Topic | Decision |
| --- | --- |
| Name | `kizz` everywhere: the binary, `kizz.md` files, the cache directory, and `KIZZ_CACHE_DIR`. |
| First version | The core builder, with `kizz execute` to observe any command and `kizz run` to run one task. Container image building comes later, as an extension of the core. |
| Platforms | Linux only. The one concession is a clean module boundary, so the core logic does not depend on how commands are run or traced. |
| Build daemons | Not supported. Their work cannot be traced; builds that need them should use another tool. |
| Unknown inputs | A step with no record has never run, so it runs. |
| Task programs | Tasks are small Starlark programs. Kizz does not run plain lists of commands. |
| Exit codes | A task program decides whether an exit code fails its task and under which conditions. A non-zero code never fails anything by itself; sometimes a negative result is the one you want, and sometimes the code does not matter. A non-zero code that the task program neither expects nor checks gets a loud warning, and `kizz build --fail-unchecked` makes it fail the task. |
| Captured output | Output can be captured as separate standard output and standard error, or interleaved. Output that is captured and then passed to another step (not piped) becomes a dependency of that step. |
| Cached results | The exit status and captured output of each step are cached and replayed to the task program on a hit. |
| Environment | Each task starts with an empty environment. Task programs read the caller environment and named `.env` files with defaults, inject variables into the environment of later commands, and clear it. The environment is a tracked input, and tasks are explicit about every variable their commands see. |
| Shared code | Task programs call functions, so common build sequences are shared and parameterized. |
| Files and data | Task programs read and write files, including structured JSON reads and writes, and those accesses are tracked. |
| Cache location | One common cache in the user's standard cache directory (`~/.cache/kizz`), stored in a key-value database. An environment variable overrides the location. |
| Cache scope | Builds started anywhere share the cache. Building deep in a tree and then several directories higher reuses the results already built below. The cache belongs to one machine and user: it is not portable between builders. |
| Database | SurrealKV. |
| Record encoding | CBOR, which suits binary data such as hashes and captured output, encoded with the `minicbor` library. |
| Lock file | Two builds cannot use the cache together: the first wins and the other waits. The waiting build is told who and what it is waiting for. The lock cleans itself up when a build stops, even if the build does not stop cleanly. |
| No git | No dependency on git or on any idea of a repository. A build starts in the current directory and collects every `kizz.md` below it. |
| Ignored `kizz.md` | Discovery uses every `kizz.md`, including ones git ignores. The helper warning for those files is deferred; see the answers below. |
| Other Markdown | Only `kizz.md` files are read. Every other Markdown file is documentation. |
| Dependency management | None. The only inputs users manage are environment variables, which cannot be handled automatically. |
| Task program in the key | The task's Starlark source is part of every step key, because it decides which commands run and in what order. Editing it re-runs the task's commands. |
| No sandbox | Task programs do I/O and run any command. Paths such as `../../../../common-file.txt` are allowed and are never treated as security violations. Users who want isolation run kizz in a container. |
| Missing files | A lookup that finds nothing (`ENOENT`) is a dependency, both so builds stay correct and so unexpected configuration-file lookups are visible. |
| Directory listings | Directory listings are dependencies, including the order in which the file system returns the entries. |
| Explanations | Kizz says exactly why it ran each command and which things changed. |
| Hermeticity | Not a goal. The goal is fast, cacheable, reliable builds without hand-written dependency lists, which nobody can maintain accurately. |
| Untrackable behavior | Assumed absent. A user who knows a command breaks that assumption marks it to run on every build. Its dependencies are still tracked, and if it changes nothing other steps depend on, the cost is time, not a broken cache. A system call that the tracer does not know makes the result not cached unless `kizz build --cache-unknown-syscalls` is given, and kizz warns about it either way. Connections to the network and to Unix sockets are detected and warned about, and the result is still cached. Machine and process state is recorded with each step and counts as an input only for a step that names it. |
| Tool awareness | None. Tool caches are tracked at file level like any other files. A tool that makes itself uncacheable is uncacheable, and kizz reports exactly why, for example that a command keeps writing a file and consuming it as an input. Users may ignore a file, glob, or regular expression, or a set of them; kizz never adds such exceptions itself. Users drive exceptions, not dependencies. |
| Inputs | Anything read during the build is an input. To exclude a file, stop the tool from reading it, or add an exception. Kizz itself stays oblivious. |
| Change detection | Different size: changed. Same size and same metadata: unchanged. Same size but different metadata: maybe changed, so compare hashes. A different type, permission bits or owner is always a change, whatever the content. A command run with `timestamps = True` also counts a different timestamp as a change. |
| Hashing | Only files that are read are hashed. For any other file, such as one that was only checked, kizz records that it was touched and its metadata only. |
| `/proc` and `/sys` | Reads are recorded, so it is known they happened, but they do not invalidate the cache. A command option lists specific `/proc` or `/sys` files whose reads do invalidate, so the user decides whether they are meaningful. |
| Concurrency | Concurrent caches, and ripgrep-style concurrent scanning wherever possible to find files, dependencies and so on. |
| Execution order | Sequential until everything works. `kizz build` has an option for it: `--sequential` is the default, and `--parallel` is an error until parallel execution is implemented. |
| Language | Rust, not Go. |
| Command line and output | The same libraries as the cbork CLI. One command, `kizz`, with subcommands; possible future subcommands are stubbed. |
| Quality gates | Clippy and the other Rust lints copied from cbork, plus Markdown linting and spell checking. |
| Build tooling | Cargo and a Justfile with the commands written out directly. `fix` fixes: it runs every automated fixer, the formatters and anything else that can be fixed automatically. `check` checks and changes nothing. `check-ci` checks and builds. `fix-ci` runs `fix` and then `check-ci`. No moon, because kizz will eventually build itself. |
| Spelling dictionary | Subagents only append words to the dictionary. The CI recipe sorts it and removes duplicates, so appends from several agents never conflict. |

The open questions of earlier drafts were answered as follows:

| Topic | Decision |
| --- | --- |
| Build without a target | `kizz build` with no target is an error that lists the available targets. |
| Exit status | 0 on success, 1 when a task fails, 2 for usage or configuration errors, and 130 when interrupted. |
| `Cargo.lock` | Committed, unlike in cbork, because kizz is a binary. |
| Untagged blocks | A `starlark` block with neither targets nor a name in a `kizz.md` gets a warning, not an error. |
| Symbolic links | Discovery follows symbolic links to directories. A link that would make it recurse is skipped with a warning, and a `kizz.md` reached through several paths runs once, under the first path in sorted order. |
| Git ignore rules | Not consulted for now. Discovery finds every `kizz.md` and uses it. |
| Target names | Letters, digits, `_`, `.` and `-`; task names use the same characters. |
| Dotenv files | No `${VAR}` expansion. |
| Writes from task programs | Only when the content differs. |
| Output decoding | Task programs get output with invalid UTF-8 replaced; the cache keeps the raw bytes. |
| Recent modification times | A recorded modification time within two seconds of when the record was written forces hashing. |
| `track` | The option that makes `/proc` and `/sys` files count is named `track`. |
| Directory entry names | Kept for listed directories with up to 1,000 entries, for explanations. |
| Environment values | Explanations name changed variables; values appear only with `--verbose`. |
| Self-inflicted re-runs | A step that re-runs in consecutive builds for the same self-inflicted reason gets a loud warning. |
| Default order | Parent directories before subdirectories; the learned order overrides it. |
| Pass limit | A build stops with an error after one more pass than there are tasks. |
| Failures | No fail-fast, and no `--keep-going`: a build tries every task. Kizz cannot tell whether a failed task failed because something it looked for was missing (`ENOENT`) or for another reason, so when a path a failed task looked for appears, the task runs again, because that path is no longer missing. When a pass runs nothing and failures remain, the build stops and fails. This first version of the rule will be refined later. |
| Cached commands in output | Counted in the summary, and listed only with `--verbose`. |
| Remote cache | Not wanted: a shared remote cache is more trouble than it is worth. |
| `graph.json` | Future. A recorded task order that a kizz run in CI, possibly without a cache, uses to run tasks in the right order from the start, instead of iterating and failing. |
| Moving a project | Rebuilds it until output restoration exists. |
| Container layers | Layer reuse is keyed on what each layer step read. |
| Cache cleanup | `kizz cache clean` removes entries for files and dependencies that existed but no longer exist, without asking, because the worst case is a rebuild. `kizz cache reset --all` wipes the whole cache, and `kizz cache reset [path]` wipes everything recorded under a path or the current directory. Both resets ask for confirmation, which `--yes` skips, and refuse to run without a terminal unless `--yes` is given. There is no separate `kizz clean`. |

## Purpose and principles

Kizz is a build engine for Linux.
It runs small Starlark programs found in `kizz.md` files, traces every command those programs run,
and records what each command actually touched as that command's dependencies.
Nobody declares dependencies; kizz observes them.

* Builds are fast, cacheable and reliable without hand-written dependency lists.
* Builds are not hermetic.
  Kizz does not sandbox or constrain anything; containers are the answer when isolation is wanted.
* Anything read during the build is an input.
  Kizz is oblivious to why something was read.
* Kizz has no knowledge of tools.
  Tools that make themselves uncacheable are reported, not special-cased.
* The user drives exceptions, never dependencies.
* Every decision is explainable: kizz can say exactly what changed and why it ran a command.
* Nothing depends on git or on a repository.
  The build starts in the current directory.
* Behavior kizz cannot observe (time, randomness, network, anything that is not a file access) is assumed absent.
  The user marks the commands that break that assumption.

## Scope

### Version 1: the core builder

The first version is the builder engine: it discovers tasks, runs their programs, runs and traces their commands,
caches the results, and explains every decision.
It ships as one command, `kizz`, with subcommands; commands planned for later are present as stubs.
`kizz execute` runs any command and reports what it touched, without a `kizz.md` and without the cache.
`kizz run` runs one task by its name, as a command runner, with the same cache.

### Later extensions

These build on the core and come after it:

* Container image building, including the layer reuse from the original outline, keyed on what each layer step read.
* Interactive commands in `kizz run`, which would give a command the terminal instead of `/dev/null` or its `stdin` text.
* Parallel execution of independent tasks behind `kizz build --parallel`, scheduled from the learned dependency graph.
* Restoring outputs from the cache instead of re-running commands, which would also let a moved project reuse its cache entries.
* An aarch64 tracer.
* A working `kizz graph`, which is a stub in version 1.
* A loud helper warning for `kizz.md` files that git might ignore.
* A `graph.json` of the learned task order, kept with the project, so kizz in CI, possibly running without a cache,
  runs tasks in the right order from the start instead of iterating through extra passes and failing.

### Non-goals

* Operating systems other than Linux.
  The only concession is a clean module boundary, so the core logic does not depend on how commands are run or traced.
* Build daemons, such as the Gradle or Kotlin daemons or an `sccache` server.
  Their work happens outside the traced process tree, so it cannot be tracked.
  Builds that need them should use another tool.
* Sandboxing, hermeticity, or restrictions on which paths a build may use.
* Built-in knowledge of any tool.
* A shared remote cache, which is more trouble than it is worth.

## Command line

```text
kizz [--color auto|always|never] [--quiet] [--no-banner] [-v | --verbose] <command>
```

Version 1 implements these commands:

* `kizz build <targets>` runs the tasks for the targets in the current directory and below.
  `<targets>` is one target or a union joined with `+`, such as `ci+check`.
  A task that matches several selected targets runs once.
  Without a target, `kizz build` exits with an error that lists the available targets.
  A build tries every task, even after a failure, and `--explain` prints the full reason for every command, cached ones included.
  `--sequential`, the default, runs one task at a time; `--parallel` exits with an error until parallel execution exists.
  `--fail-unchecked` fails a task whose command exited with a non-zero code that the task never checked
  (see [Exit codes and failure](#exit-codes-and-failure)).
  `--cache-unknown-syscalls` caches the results of commands that made a system call that the tracer does not know,
  which are not cached by default; kizz warns about them either way (see [The Linux tracer](#the-linux-tracer)).
* `kizz why [path]` prints why each command ran or was cached in the most recent build,
  for tasks under `path`, which defaults to the current directory.
* `kizz list [targets]` lists discovered tasks with their file, line, heading, targets and options, plus any discovery warnings.
* `kizz cache clean` removes stale entries from the shared cache, and `kizz cache reset` wipes the entries under a path,
  or the whole cache with `--all`, after confirmation (see [Cleaning up the cache](#cleaning-up-the-cache)).
* `kizz execute --output=<file> -- <program> [arguments...]` runs any program and reports what it touched in `<file>`.
  `--compact` combines the observations of every process, and `--json` writes JSON instead of text.
  It needs no `kizz.md` and no cache; see [Observing any command](#observing-any-command-with-kizz-execute).
* `kizz run <name> [-- <arguments>...]` runs the task named `<name>` once, showing each command and its output live.
  `--no-cache` runs every command, and `--explain`, `--fail-unchecked` and `--cache-unknown-syscalls` work as for `kizz build`.
  See [Running one task with kizz run](#running-one-task-with-kizz-run).

Parsed but stubbed, in the same style as the cbork CLI stubs: `kizz graph` and `kizz image` (container images).

Output follows cbork: a banner suppressed by `--quiet` or `--no-banner`, with styling from `console`.
`kizz execute` never prints the banner, writes nothing to standard output, and writes only its fatal errors to standard error.
Warnings are loud: bold, colored, and repeated in the final summary.
`kizz build` prints one line for each command it runs, with a one-line reason.
Cached commands are counted in the summary and listed with `--verbose`.

Exit status: 0 on success, 1 when a task fails, 2 for usage or configuration errors, and 130 when interrupted.
`kizz execute` exits with its program's status instead, as [Observing any command](#observing-any-command-with-kizz-execute) says.

## Discovery

* Discovery starts in the current working directory and never looks above it.
* It finds every file named exactly `kizz.md` in that directory and every subdirectory, hidden directories included.
* It uses a concurrent directory walk of its own, on kizz's worker pool, with all filtering off.
* It does not consult git or its ignore rules: every `kizz.md` it finds is part of the build.
* Other Markdown files are documentation and are never read.
* Symbolic links to directories are followed, except a link that would make the walk recurse into one of its own parents.
  Such a link is skipped with a warning, and a `kizz.md` reached through several paths runs once,
  under the first of those paths in sorted order.
* Files are handled in sorted path order, so discovery is deterministic.

## Tasks

A task is a fenced code block in a `kizz.md` whose info string is `starlark` followed by targets, task options or both:

````markdown
## Build the library

```starlark ci check name=build
env.inherit("PATH", "HOME")
build = run("cargo", "build", "--locked", stream = True)
if build.rc != 0:
    fail("cargo build failed")
```
````

* Target names use letters, digits, `_`, `.` and `-`.
* After `starlark`, a word with `=` is a task option, and any other word is a target.
* `name=<name>` names the task, with the characters of a target name, so that `kizz run <name>` can run it.
  Each name is unique within its `kizz.md`, and a name given to two tasks of one file is a configuration error.
* `cache=never` makes every command of the task run on every build, as if each of its `run()` calls had `always = True`.
* An unknown option, an option given twice, or a value that the option does not accept is a configuration error.
* A task with a name and no targets is selected only by `kizz run`, never by `kizz build`.
* Task options are not part of the task program, so they change no step key.
* The nearest heading above the block is the task's heading, and output shows the task as `path/kizz.md:LINE (Heading)`.
* A task is identified by the absolute path of its `kizz.md` and its position among the task blocks in that file.
* A `starlark` block with neither targets nor a name is not a task, and kizz warns about it because it is probably a mistake.
  Its task options are still checked, so a mistake in them is still a configuration error.
* Backtick and tilde fences are both recognized.
* Each task is one Starlark program.
  The block's source plus every module it loads is the *task program*.
  The task program's hash is part of the cache key of every step in the task,
  because the program decides which commands run and in what order.
  Editing the program re-runs the task's commands.

## The task API

Task programs are Starlark, extended with functions for running commands, managing the environment, and reading and writing files.
Kizz does not run plain lists of commands.
A task program runs commands, captures their output, inspects their exit codes, and decides what happens next.

### Language

* Starlark through starlark-rust, with its extended dialect so programs can use top-level `if` and `for`.
* `fail(message)` fails the task.
  Output from `print(...)` is shown, prefixed with the task's name, or its heading when it has none, or its location.
* Starlark's `json.encode` and `json.decode` are available for strings.
* There is no sandbox: task programs read and write files, read the caller environment, and run any command.

### Running commands

```starlark
result = run(
    "cargo", "build", "--release",  # argv: the program, then its arguments
    stdin = None,       # text for standard input; None connects /dev/null
    env = None,         # extra variables for this run only; a None value removes a variable
    cwd = None,         # directory relative to the task directory; None means the task directory
    capture = "split",  # "split": stdout and stderr separately; "merged": one stream in exact order
    stream = False,     # also echo output live to the terminal
    always = False,     # run on every build, even when cached
    ignore = [],        # user exceptions: paths not recorded as dependencies of this step
    track = [],         # /proc and /sys files whose content counts as an input of this step
    timestamps = False, # also compare the timestamps of what the step read, ran, listed or checked
    machine = [],       # machine and process state that counts as an input; see Machine state
    expect = None,      # None: never fail the task on exit status; or a list of acceptable exit codes
)
```

* Kizz runs the program directly, without a shell.
  Use `run("sh", "-c", "...")` when shell features are wanted.
* Each positional argument is a string, or a list or tuple whose elements are strings, lists or tuples.
  Each list or tuple expands in place, in order and at any depth, so the command is the flat sequence of its strings.
  An empty list or tuple adds nothing, and each string stays exactly one argument, never split on spaces.
  So `run("echo", ["this", "is", "test"])` and `run(["echo", "this"], "is", "test")` are the same step, with the same key.
  A program can build its arguments by logic, and an optional argument can be a variable that holds `[]` when not wanted.
  After the expansion the first string is the program, and an expansion without any string fails as a call without a program.
  Any other value fails the task with an error that names its argument, its index path within the lists, and its type.
  A list or tuple that contains itself, directly or through other lists or tuples, fails the task with an error.
  An expansion that would visit more than 1,000,000 values, counting strings, lists and tuples, fails the task.
  The step key, the printed command, explanations, `kizz why`, reports and the cache all see the expanded command.
* A program name without `/` is looked up in the `PATH` of the task environment, never in kizz's own.
  Every location kizz checks is recorded as a dependency of the step, so a program that appears earlier in `PATH` re-runs the step.
  If the task environment has no `PATH`, a bare program name is an error that says to set or inherit `PATH`.
* Output is always captured; `capture` decides how.
  `merged` sends both streams through one pipe, so the order is exact.
* The result has `rc` (the exit code, or `None` when a signal killed the process), `signal`,
  `stdout` and `stderr` (with `split`), `output` (with `merged`), and `cached` (whether the result came from the cache).
* Output is decoded as UTF-8 for the program, with invalid bytes replaced; the cache stores the raw bytes.
* With `timestamps = True`, a new timestamp of a file or directory that the step used runs it again,
  even when the content is the same, for tools such as `tar` and `zip` that store timestamps.
  See [Cache check](#cache-check).

### Exit codes and failure

A non-zero exit code never fails a build by itself.
The task program decides whether an exit code matters, which codes are acceptable, and what happens next.

* With `expect = None`, the default, `run()` always returns and the program inspects `rc`.
* With `expect = [0, 1]`, the task fails when the exit code is not in the list or a signal killed the process.
  Kizz then prints the command, its exit status and its captured output.
* `fail()` fails the task explicitly.
* A command's non-zero exit or signal is unchecked when its `run()` has no `expect` and the program never checks the result.
  The program checks a result by reading its `rc` or `signal`, as an attribute or with `getattr`, or by encoding it.
  Reading only the result's output or `cached` does not check it.
  `json.encode` and `fs.write_json` encode a result as an object of its attributes and their values.
  Encoding a result checks it: its exit status then leaves the program on purpose, for example in a file for a later step.
* Once the build settles, kizz warns loudly about each unchecked command in the results that stand.
  The results that stand are those of each task's latest run or replay in the build.
  A task that runs again in a later pass replaces its earlier results, so a superseded result never warns.
  Each warning names the task, the command and how it ended, and the summary repeats it.
  A result replayed from the cache of an earlier build warns again, and the warning says that it came from the cache.
  A build that does not settle, or that is interrupted, warns about no unchecked command.
* `kizz build --fail-unchecked` turns that warning into a task failure when the task program ends.
  Kizz then prints the command, its exit status and its captured output, as for an unexpected exit.
  As for any failed task, a failure that a later pass replaced does not fail the build.

### Captured output is cached and becomes a dependency

A step's exit status and captured output are stored with its cache entry.
On a cache hit, kizz returns them to the program exactly as recorded,
so the program makes the same decisions without re-running the command.

Captured output can be passed to a later step, captured and then sent rather than piped: as arguments, as `stdin`,
as environment variables, or through a file the program writes.
Arguments, `stdin` and the environment are part of the later step's cache key, and written files are recorded inputs,
so the captured output becomes a dependency of the step that consumes it.
When it changes, exactly the consuming steps run again.

### Environment

Every task starts with an empty environment.
Nothing reaches a command unless the task puts it there, so each task is explicit about every variable its commands see.
The exact environment passed to each command is part of that step's cache key, so the environment state is tracked precisely.

* `env.caller(name, default = None)` reads a variable from the caller environment without changing the task environment.
* `env.inherit(name, ...)` copies variables from the caller environment into the task environment; unset ones are skipped.
* `env.load_file(path, defaults = {}, required = True)` reads a named dotenv file,
  fills in `defaults` for variables the file does not set, adds the result to the task environment, and returns it as a dict.
  The file is recorded as a dependency of the task.
  With `required = False`, a missing file means only the defaults apply.
* `env.read(path)` reads a dotenv file into a dict without changing the task environment.
* `env.set(name, value)`, `env.update(values)`, `env.unset(name)`, `env.get(name, default = None)` and `env.dict()`
  work on the task environment.
* `env.clear()` empties the task environment.

Dotenv files use `NAME=value` lines, `#` comments, an optional `export` prefix, and single or double quotes;
double quotes support `\n`, `\t`, `\"` and `\\`.
There is no `${VAR}` expansion, because expansion would silently pull values from elsewhere.

### Files and JSON

Task programs read and write files directly, so a build can adapt to its own data.
Paths are relative to the task directory; absolute paths and paths outside the tree are allowed.

* `fs.read(path)` returns text, and `fs.write(path, text)` writes text, creating parent directories.
* `fs.read_json(path)` returns a Starlark value, and `fs.write_json(path, value, indent = 2)` writes stable JSON
  with a trailing newline.
* `fs.exists(path)`, `fs.list(path = ".")` (sorted names) and `fs.glob(pattern)` (sorted matches) inspect the file system.
* Writes happen only when the content differs, so unchanged files keep their timestamps.
  A write that changes a file counts as work in its pass, as a command that runs does, so another pass follows.
  Kizz records the task as the file's last writer, for the learned order and for explanations.

The task program runs on every build, so its own reads always see current data.
Kizz still records every file access the program makes, including missing files and directory listings,
and uses those records for task ordering, the dependency graph and explanations.
Whatever the program derives from its reads reaches commands through arguments, `stdin`, the environment or files,
and is tracked there.

### Shared code

* Common build sequences are ordinary Starlark functions, parameterized however their authors want.
* `load("path/to/lib.star", "name")` imports from a Starlark module file.
  The path is relative to the loading file, and absolute paths are allowed.
* Functions from modules act on the task that calls them: their `run()`, `env` and `fs` calls belong to that task.
* Calling `run()` while a module is being loaded, outside any task, is an error.
* Every module a task loads, directly or indirectly, is part of the task program hash,
  so editing a shared function re-runs the commands of every task that loads it.

### Task information

These members of `task` describe the running task:

* `task.dir`: the directory of its `kizz.md`, the task directory
* `task.file`: the absolute path of its `kizz.md`
* `task.line`: the line of the block's opening fence, counting from 1
* `task.name`: the name that its `name=` option gives, or `None`
* `task.heading`: the nearest heading above the block, or `None`
* `task.targets`: the targets on the block
* `task.selected`: the requested targets that selected the task
* `task.args`: the task's arguments, as a list of strings, which is empty under `kizz build`

`task.name` was the heading before tasks had names; the heading is now `task.heading`.
The arguments are not part of the task program hash: they reach commands only through what the program does with them.

### Example

````markdown
# Server

## Release build

```starlark ci
load("../build/common.star", "go_build")

env.inherit("PATH", "HOME")
env.load_file(".env", defaults = {"GOFLAGS": "-trimpath"}, required = False)

described = run("git", "describe", "--tags", "--always", capture = "merged", expect = [0])
version = described.output.strip()
fs.write_json("dist/version.json", {"version": version})

go_build("./cmd/server", output = "dist/server", version = version)

tests = run("go", "test", "./...", stream = True)
if tests.rc != 0:
    fail("tests failed:\n" + tests.stderr)
```
````

The shared module, `build/common.star`:

```starlark
def go_build(package, output, version):
    return run(
        "go", "build", "-o", output,
        "-ldflags", "-X main.version=" + version,
        package,
        expect = [0],
    )
```

## Dependency tracking

### Tracer boundary

The core never starts a process itself and knows nothing about system calls.
It asks a tracer to run one command with an exact program, arguments, environment, working directory and `stdin`.
A request can instead let the command keep kizz's standard streams and process group, and the caller can forward signals to it.
The tracer returns the exit status, the captured output, and every path the process tree touched, with how it touched it.
A request can also ask for the process tree, which [Processes and outcomes](#processes-and-outcomes) describes.
Through the same interface, the core reads what the standard library cannot: the extended attributes of a path,
and the machine and process state that a command starts with.
The core also asks it for the name of an error number, such as `ENOENT`, to report a failed check of its own.
Version 1 has one tracer, for Linux, and tests use a fake one.
Nothing in the core changes when the tracer does.

### The Linux tracer

* It uses `ptrace` with a seccomp-bpf filter in front.
  The filter stops the process only for system calls that touch the file system or that the tracer does not know,
  and lets everything else run at full speed.
  Seccomp alone cannot read path arguments or see results, so `ptrace` reads the path at entry and the result at exit.
* It follows the whole process tree, including threads, forks and `exec`,
  and the traced processes are killed if kizz dies (`PTRACE_O_EXITKILL`).
* A command of a build runs in a process group of its own, with pipes or `/dev/null` as its standard streams.
  A terminal's Ctrl-C then reaches only kizz, which interrupts the command exactly once.
* A command that keeps the terminal, as `kizz execute` runs it, uses kizz's standard input, output and error directly.
  It stays in kizz's process group and session, so the terminal's input and signals reach it as they would without kizz.
  Kizz captures none of its output.
* Every command starts with an empty signal mask and the default action for `SIGPIPE`, which kizz itself ignores.
* A signal that kizz forwards goes to the command's first process while it lives, and then to each process still running.
* The tracer resumes a process that a stop signal stopped, since it cannot tell when `SIGCONT` would end the stop.
* At each `exec` it also records what the kernel loads without another system call: the interpreter on a `#!` line,
  the interpreters of interpreters that are scripts themselves,
  and the loader that an ELF executable names in its `PT_INTERP` program header.
  A relative interpreter path is resolved against the working directory, as the kernel resolves it.
  These paths get the exec kind when the `exec` succeeds, and a probe when it fails.
* A program that runs although the tracer cannot read it, or that is neither a `#!` script nor an ELF executable
  for the machine, marks the run as incompletely traced, since a handler such as `binfmt_misc` may run it.
* It resolves relative paths against the calling thread's working directory or the directory descriptor the call used.
* It records successful and failed calls alike; a lookup that fails with `ENOENT` is a dependency.
* An `openat2` with `RESOLVE_IN_ROOT` or `RESOLVE_BENEATH` and without `O_NONBLOCK` that fails with `EAGAIN` records nothing.
  Linux gives that error when a rename or mount anywhere on the machine races with a `..` step, and abandons the lookup.
  The caller may repeat the call, and each repeated call is recorded like any other.
  With `O_NONBLOCK`, the error can also mean that a lease refused a file that the lookup found, so it is recorded as usual.
* `io_uring_setup` fails with `ENOSYS`, so programs fall back to ordinary system calls,
  because `io_uring` operations would bypass the filter.
* System calls made through a non-native interface (32-bit `int 0x80` or x32) mark the run as incompletely traced:
  the result is not cached and kizz warns loudly.
* Paths are reported as absolute paths with `.` components and repeated separators removed.
  `..` components are kept, because collapsing them is wrong when part of the path is a symbolic link.
* x86_64 comes first; aarch64 is a later extension.
* Each tracked call costs two stops.
  Riker, which uses the same approach, added a preload library to avoid stops for common calls.
  Kizz measures first, and any fast path sits behind the tracer boundary.

Tracked system calls, by x86_64 name:

* opening files: `open`, `openat`, `openat2` and `creat`
* metadata and existence: `stat`, `lstat`, `newfstatat`, `statx`, `access`, `faccessat`, `faccessat2`,
  `file_getattr` and `name_to_handle_at`
* file systems: `statfs`, and `fstatfs` on the file system behind a descriptor
* watches: `inotify_add_watch` and `fanotify_mark`
* extended attributes: `getxattr`, `lgetxattr`, `fgetxattr`, `getxattrat`, `listxattr`, `llistxattr`, `flistxattr`,
  `listxattrat`, `setxattr`, `lsetxattr`, `fsetxattr`, `setxattrat`, `removexattr`, `lremovexattr`, `fremovexattr`
  and `removexattrat`, those on a descriptor resolved to the path behind it
* links: `readlink`, `readlinkat`, `link`, `linkat`, `symlink` and `symlinkat`
* directory listings: `getdents` and `getdents64`
* programs: `execve` and `execveat`
* working directory: `chdir` and `fchdir`
* changes: `rename`, `renameat`, `renameat2`, `unlink`, `unlinkat`, `rmdir`, `mkdir`, `mkdirat`, `mknod`, `mknodat`,
  `truncate`, `chmod`, `fchmodat`, `fchmodat2`, `chown`, `lchown`, `fchownat`, `utime`, `utimes`,
  `utimensat`, `futimesat` and `file_setattr`
* descriptor-based changes: `ftruncate`, `fchmod`, `fchown` and `fallocate`,
  resolved to the path behind the descriptor
* sockets: `bind`, `connect`, `sendto`, `sendmsg` and `sendmmsg`

Reading or listing extended attributes records the xattr kind, also when the call fails
because the attribute does not exist (`ENODATA`), the buffer is too small (`ERANGE`) or the file system has none (`EOPNOTSUPP`):
each of those answers depends on the attributes.
Setting or removing an attribute is a write.
`file_getattr` and `name_to_handle_at` record a probe, and so do `inotify_add_watch` and `fanotify_mark`, which watch a path.
`file_setattr` is a write; the file attributes that `file_getattr` returns, such as the immutable flag, are not compared.
`statfs` and `fstatfs` record a probe; the figures they return, such as free space, are not compared.
Binding a Unix socket to a path writes the path, and connecting or sending to one probes it,
so a missing socket is a dependency like a missing file.
`connect`, and `sendto`, `sendmsg` and `sendmmsg` with a destination, also record the step's connections:
each IPv4 or IPv6 address and port whatever the outcome of the call, and each Unix socket, by path or abstract name,
unless its path was not found.
Other socket families, such as netlink, which talks to the kernel, are not recorded.

System calls that mark the run as incompletely traced, by x86_64 name:

* opening by handle: `open_by_handle_at`
* changing the root: `chroot`, `fchroot` and `pivot_root`
* mounting: `mount`, `umount2`, `fsopen`, `fsconfig`, `fsmount`, `fspick`, `move_mount`, `open_tree`,
  `open_tree_attr` and `mount_setattr`
* mount namespaces: `unshare`, `setns`, `clone` and `clone3`
* other uses of a path: `uselib`, `acct`, `quotactl`, `quotactl_fd`, `swapon` and `swapoff`

The first four groups can change what a path means, so the paths kizz recorded may not be the ones the command used.
The last group uses a path in ways that kizz does not record: loading a library, writing process accounting,
and managing disk quotas or swap space.
A process that makes one of them marks the run when it makes the call, whether or not the call succeeds.
`unshare`, `clone` and `clone3` count only with the flag for a new mount namespace, `CLONE_NEWNS`,
and `setns` only when it can enter a mount namespace: with that flag, or with no namespace type.
The result is not cached, and kizz warns loudly and names the call.

Kizz needs Linux 5.4 or later: the tracer collects its first process by descriptor (`pidfd_open` and `waitid` with `P_PIDFD`).
The tracer knows the native system calls of the x86_64 table of Linux 7.3, numbers 0 through 336 and 424 through 472.
Those that this section does not name run without a stop,
so a file-system operation that none of the named calls makes is not recorded.
Any other native number is an unknown system call, which a newer kernel may have given a meaning the tracer cannot judge.
The filter stops for it, and the tracer reports its number when a process makes it, then lets the call run.
By default, a run that makes an unknown system call is treated as incompletely traced: its result is not cached.
`kizz build --cache-unknown-syscalls` caches such a result anyway.
Either way, kizz warns loudly when the command runs, naming the command and each unknown call by number,
and repeats the warning in the summary; `kizz why` lists the unknown calls of each step that made any.
The option is not part of the step key, so a result cached with it is reused by later builds with or without it,
until something that the step recorded changes.

### Processes and outcomes

A request can ask the tracer for the process tree of the run, besides the combined accesses that builds record.
Builds never ask for it, so they record and compare exactly what they did before.
`kizz execute` asks for it, and so does `kizz run` when it writes a report.
Neither the tree nor its outcomes are stored in the cache.

* Each traced process is a thread group: its threads fold into it.
* A process has a number, unique within the run, its process ID, and the number of the process that created it.
* Processes are numbered from 1 in the order the tracer saw them start, and process 1 is the command.
* A process ID that the system gives to a new process within the run starts a new process, so two processes never merge.
* A process lists the programs it ran, in order, and the status it ended with, when the tracer saw it end.
* A program is the path that an `exec` named, resolved as its observation is, with the arguments that the call passed.
* A process created by a fork, a vfork or a clone first runs its creator's program, which it lists as inherited.
* An inherited program is listed only when something is attributed to it.
* When the command cannot start, its process lists the program that kizz tried to start, without a status.

Everything the tree records belongs to the program that its process ran when the call was made.
A successful `exec` belongs to the program that it starts: that program's own path, its interpreters and its ELF loader.
A failed `exec` belongs to the program that made the call.
Connections, untraceable calls and unknown system calls belong to programs in the same way.

An observation is a path with its access kinds, and each kind with its outcomes.
An outcome is `ok`, the name of the error that a failed call returned, such as `ENOENT`, or `unknown`.
A call whose result the tracer never saw, because its thread was killed during the call, has the outcome `unknown`.
An error number without a name is written as `errno` and the number, such as `errno 200`.
The kinds of each path in the tree, combined over every program, are exactly that path's kinds in the combined accesses.
The tree lists every reason that the trace is incomplete, each with its program, while the combined result names the first.

### What is recorded

For each step, kizz records every path the command touched, combining the kinds of access for each path:

* read: content was read
* probe: existence or metadata was checked, including lookups that found nothing
* list: a directory's entries were read
* write: the path was created, modified, renamed, removed or had its metadata changed
* exec: the path was run as a program, or the kernel loaded it to run one, as a `#!` interpreter or an ELF loader
* xattr: the path's extended attributes were read or listed

When the command finishes, kizz records the state of every touched path, concurrently.
The next cache check compares the current state with that recorded state, so a file a step writes and then reads,
or a temporary file it creates and removes, needs no special handling.

The final state can differ from what the command consumed: a command may rewrite a file after it read it,
or something else may change the file while the command runs.
So the tracer also notes the metadata of each regular file when the command first opens it for reading or runs it,
unless the command wrote that path earlier in the run, or the call that opened it created or truncated it.
When the command finishes, a file whose state then differs from that metadata makes the result not cached
(see [Results that are not cached](#results-that-are-not-cached)).
Directory listings are compared by their final state only.

### Path states

* Absent.
* Regular file that was read, including a program that was run: size, modification and change times in nanoseconds,
  inode, device, mode (its type and permission bits) and owner (user and group), plus a BLAKE3 hash of its content.
* Any other regular file, such as one that was only checked or only written: that the command touched it, and the same metadata.
  Kizz never hashes it.
* Directory: its mode, owner, and modification and change times.
  When listed, also a hash of its entry names and types in the order the file system returned them,
  plus the entries themselves when there are not too many
  (up to 1,000), so explanations can name what appeared or disappeared, or say that only the order changed.
  Entry inode numbers are not recorded.
* Symbolic link: the link target, plus the state of what it points to.
* A path whose extended attributes were read or listed also records them: their names and a hash of each value,
  for the path itself and, for a symbolic link, for what it points to.
* Device, FIFO or socket: type only.
* Anything under `/proc` or `/sys`: recorded so explanations and `kizz why` show the access,
  but never compared when kizz checks the cache unless the user tracks it, as described below.

### `/proc` and `/sys`

These are kernel interfaces, not files, and many of them change on every read.
Kizz records every access to them, so it is known that a command used them, but by default they never invalidate the cache.
The user decides which ones are meaningful: `run(..., track = [...])` lists `/proc` and `/sys` files,
using the same pattern forms as `ignore`, whose content then counts as an input of that step.
Kizz hashes a tracked file when it records the step and reads it again when it checks the cache, so a change re-runs the step.
Per-process paths such as `/proc/self` describe kizz when it checks the cache rather than the command,
so tracking them is rarely meaningful.
`track` patterns are part of the step's cache key.

### Machine state

The state of the machine, and the process state that a command inherits, can change what the command does.
Kizz never compares it by itself: it observes it when a command starts and records it with the step,
and `kizz why` shows the parts that a step names, and every part with `--verbose`.
`run(..., machine = [...])` names the parts that count as an input of the step:

* `kernel`: the kernel release and version
* `limits`: every resource limit, soft and hard
* `affinity`: the CPUs the command may run on
* `memory`: the total and the available memory
* `filesystem`: the type and mount flags of the file system that holds the working directory
* `disk`: the size and free space of that file system

A step runs again when a value of a part it names differs from the recorded one,
and the explanation names each changed value with its old and new value.
Available memory and free space change on almost every build, so a step that names `memory` or `disk` runs on almost every build.
The names are part of the step's cache key, and an unknown name fails the task.
The host name, the credentials, the umask, the machine architecture and the CPU model are not per step:
they are the [cache identity](#cache-identity).

### Behavior kizz cannot see

Kizz assumes a command does nothing it cannot observe: no dependence on time, randomness, the network, process IDs,
or anything else that is not a file access.
When the user knows a command breaks that assumption, for example because it may download something,
they mark it with `always = True`.
The command then runs on every build, and its dependencies are still recorded.
If it does not change anything another step depends on, the cost is time, not correctness: dependent steps compare content hashes,
find nothing changed, and stay cached.

Kizz detects connections to the network and to Unix sockets, because what comes back is an input it cannot see.
It warns loudly about each connection, naming the address or socket, and repeats the warning in the summary.
The result is still cached, and `kizz why` lists the step's connections.
Kizz warns when the command runs, not on a later cache hit.
Kizz also assumes that a command has no effect outside the files it writes.
A cache hit does not repeat an upload, a database update or a notification,
so a command whose effect must happen on every build is marked `always` as well.
Devices, FIFOs and sockets are compared by type only, never by what they deliver or by the service behind them.

### User exceptions

The user, never kizz, decides what to leave out.
`run(..., ignore = [...])` takes a list of patterns, and matching paths are not recorded for that step.
A pattern is a path, a glob, or a regular expression prefixed with `re:`, matched against absolute paths.
Relative globs are anchored at the task directory, and a leading `~/` means the caller's home directory.
Starlark variables make reusable sets, such as `GO_CACHE = ["~/.cache/go-build/**"]`.
Ignore patterns are part of the step's cache key, and `kizz why` reports how many accesses each step ignored.
Kizz never adds exceptions of its own.

## Cache

### Location

* The cache is `KIZZ_CACHE_DIR` when set, otherwise `$XDG_CACHE_HOME/kizz`, otherwise `~/.cache/kizz`.
* One cache serves every build on the machine, wherever it starts.
* Cache keys use the absolute directory of each task, not the directory kizz was started in.
  Running `kizz build ci` deep in a tree and then again four directories up reuses the results already built below.
* Moving a project to a new path means building it again;
  keys that survive a move need output restoration, which is a later extension.
* The cache only grows during builds; `kizz cache` cleans it up, as described in [Cleaning up the cache](#cleaning-up-the-cache).

### Cache identity

The cache is not portable: it belongs to the machine and the user that created it.
When kizz creates the database, it stamps it with the host name, the credentials
(the effective user and group and the supplementary groups), the umask, the machine architecture and the CPU model.
Every later command that uses the cache compares the stamp with the current values.
If any of them differs, kizz refuses to use the cache, names each value that differs, and exits with status 2.
`kizz cache reset --all` still wipes such a cache, and the next build stamps the new one; another cache, set with `KIZZ_CACHE_DIR`,
avoids the conflict.
The kernel, resource limits, memory and the rest of the machine state are per step; see [Machine state](#machine-state).

### Database

The cache is a key-value store: SurrealKV, embedded, in the `db` directory of the cache.
Records are CBOR, encoded with `minicbor`; CBOR stores binary data such as hashes and captured output directly.
The store holds these records:

* step records, by step key: the full step specification for explanations, the observations with their recorded states,
  the exit status, the captured output, the duration, when the step ran and why it ran
* step positions: the step key each task used last time at each step position,
  so kizz can explain what changed when the key itself changed
* task records: the task program hash and a summary of the task's last run
* last writers: for each path, the step or the task program that wrote it most recently
* learned order: for each task, the tasks whose outputs it consumed
* a schema version
* the cache identity: the stamp of the machine and user that the cache belongs to

The database is shared by concurrent threads within a build; SurrealKV provides concurrent transactions.
The store's runtime runs SurrealKV's work on the threads that use the store and starts no thread of its own.
Kizz builds SurrealKV from a copy in `vendor/surrealkv`, patched so that it never panics when a thread cannot start.
The store opens SurrealKV with the patch's clock option, so SurrealKV starts no clock thread.
That clock reads the system clock directly on every reading, and each reading is greater than the one before.
SurrealKV's write pacing is turned off, with its write stalls, and the patched pacing starts no thread.
To replay several log segments, SurrealKV starts a thread when it can, and otherwise replays them on the calling thread.
`vendor/surrealkv/KIZZ-PATCHES.md` lists each change against SurrealKV 1.0.0, with its reason, to offer upstream.

### Lock file

* The cache directory holds `kizz.lock`.
  A build holds an exclusive lock on it (`flock`) for its whole run, so only one build uses a cache at a time.
* `kizz run` holds the lock for its whole run, as a build does.
* The first build wins and any other build waits.
* The holder writes its details into the file: process ID, user, host, working directory, command line and start time.
* A waiting build prints those details at once, and prints them again if a different build takes the lock while it waits.
* The kernel releases the lock when the holding process exits for any reason, including a crash or `SIGKILL`,
  so a dead build never blocks another.
* On a normal or interrupted exit, kizz also deletes the file, and a file left behind by a crash is taken over by the next build.
* After taking the lock, a build checks that the file it locked is still the file at that path, and retries if not.
  This closes the race between one build deleting the file and another build locking it.
* Builds that need to run at the same time can use separate caches through `KIZZ_CACHE_DIR`.

### Step keys

A step's key is a BLAKE3 hash, derived with a versioned context string, of a CBOR encoding of these values written with `minicbor`:

* the task program hash, covering the block and every loaded module
* the absolute working directory
* the arguments
* the environment passed to the process
* the `stdin` content
* the capture mode
* the ignore patterns
* the `track` patterns
* whether `timestamps` is set
* the `machine` names
* the occurrence number, which counts identical commands earlier in the same task run,
  so a command repeated in a task gets one cache entry per repetition

The encoding is deterministic: definite lengths, fields in a fixed order and maps sorted by key,
so the same step always produces the same key.

`always`, `stream` and `expect` are not in the key, because they change what kizz does with a step, not what the command does.
`timestamps` is in the key, because a record made without it keeps the timestamps of its latest cache hit.

### Cache check

1. Look up the step key.
   With no record, the step has never run with this exact specification, so it runs.
2. With a record, compare every recorded observation with the current file system, concurrently.
   If everything matches, the step is a cache hit.
   If anything differs, the step runs, and the explanation lists every difference.

A regular file that was read is compared like this:

* Different type, permission bits or owner: changed, without hashing.
  A command may check them, so the same content does not make them unchanged.
* Different size: changed, without hashing.
* Same size and same metadata (modification and change times, inode, device, mode and owner): unchanged.
* Same size, mode and owner, but different times, inode or device: maybe changed, so kizz hashes the file.
  If the hashes match, the file is unchanged, and kizz stores the new metadata so the next check takes the fast path.
* If the recorded modification time is within two seconds of when the record was written,
  metadata cannot prove the file unchanged, so kizz hashes it, as git does for racily clean files.

A regular file that was not read has no hash, so its metadata is the whole dependency:
any difference in size or metadata is a change.
A directory's mode and owner are always compared.
A listed directory is listed again and compared by its entries, in the order the file system returns them.
A path whose extended attributes were recorded is compared by them too: an attribute that appeared,
disappeared or changed is a change.
Untracked paths under `/proc` and `/sys` are never compared.

Programs cannot set the change time, so a tool that restores modification times
(`tar`, `rsync -t` or `cp -p`) still trips the metadata check.

With `run(..., timestamps = True)`, the bytes do not decide alone.
A different modification or change time of a file or directory that the step read, ran, listed or checked is a change,
found without hashing.
A new or removed hard link changes a file's change time, so it also runs such a step again.
Access times are never compared, because reading a file changes them.
Neither are the times of a symbolic link itself.

During a build, file states and hashes are held in a concurrent in-memory cache,
so a file that many steps depend on is hashed once per build.
An entry is dropped when a step records a write to its path.
Hashing uses BLAKE3, multi-threaded for large files.
Hashing and the other parallel work of a command run on kizz's worker pool, as [Kizz never panics](#kizz-never-panics) says.

### Results that are not cached

* Commands killed by a signal, including interrupted commands.
* Runs that the tracer reports as incompletely traced.
* Runs that made a system call that the tracer does not know, unless `kizz build --cache-unknown-syscalls` is given.
* Runs in which a file changed after the command first read it or ran it.
  The explanation names each such file and says whether the command itself wrote it.
* Commands that could not start for a reason other than a missing or non-executable program.

These run again on the next build, with that stated as the reason.
A run in which something other than the command changed a file it read gets a loud warning.
Within a build, kizz keeps the paths such a command touched in memory, so a later pass reruns it only if one of them changed.
A tracer failure counts as a command that ran, and it leaves no record.
Within a build, kizz keeps such a failure in memory too, so no later pass runs that command again.

A command whose program is missing or not executable fails its task and is recorded like any other failure,
with every path kizz checked while looking for the program.
It runs again once the program appears, for example after another task builds it.

### Commands that always run

An `always` command runs at most once per build, when a pass first reaches it, and is recorded like any other step.
That is normally the first pass; later passes of the same build reuse its result without running the command again.
A command first reached in a later pass, or with a new step key, therefore runs once in that pass.
If it rewrites files with identical content, dependent steps see unchanged hashes and stay cached.
If it changes content, dependent steps run again, as they should.
Every command of a task marked `cache=never` runs as if it were marked `always`, and its reason names the task option.
Nothing makes such a command cached.
`kizz run --no-cache` runs every command of its task, also one that a cache hit would replay, and its reason says so.

### Cleaning up the cache

The shared cache only grows during builds.
These commands clean it up.
They change cache entries only, never project files.

* `kizz cache clean` removes entries about files and dependencies that existed when kizz recorded them but no longer exist.
  It scans every recorded path concurrently.
  A step record that depends on such a file is removed as a whole,
  because dropping a single dependency from a record would let the record match when it should not.
  Turning the dependency into a missing-file dependency would be wrong as well,
  because the recorded result was produced while the file existed:
  a step that read the file would replay output its command can no longer produce,
  and a step whose output file was deleted would never run again.
  Task records, step positions and learned order for `kizz.md` files that no longer exist are removed too,
  as are last-writer entries for paths that no longer exist.
  Dependencies on files that did not exist when they were recorded are kept, because they are still valid.
  It asks for no confirmation, because it removes only entries for files that are gone; the worst case is a rebuild.
* `kizz cache reset --all` wipes the whole cache, also one that kizz refuses to use because of its identity.
* `kizz cache reset [path]` wipes every entry for tasks under the path, which defaults to the current directory:
  their step records, step positions, task records and learned order, and the last-writer entries for paths under it.
* Both resets show what they will remove, such as the number of tasks and records, and ask for confirmation first.
  `--yes` skips the question, and without a terminal a reset refuses to run unless `--yes` is given.
* Every cache command takes the cache lock, so none runs during a build,
  and a waiting command says what it is waiting for, as a waiting build does.

## Explaining decisions

Kizz records why every command ran and prints it, so nobody has to guess why something keeps rebuilding.

These are the reasons:

* never run: there is no record for this step
* the task program changed: its block, or a named loaded module
* the arguments changed: old and new, word by word
* the environment changed: which variables were added, removed or changed,
  with values shown only with `--verbose`, because values can be secrets
* the working directory, `stdin`, capture mode, ignore patterns, `track` patterns, `timestamps` or `machine` changed
* inputs changed: every changed path, what changed about it
  (it appeared or disappeared, or its content, size, type, permissions, owner, timestamps, extended attributes,
  directory entries, entry order or link target changed),
  and who last wrote it: this command on its previous run, another task's command or program in this build,
  a command or task program of an earlier build, or something outside kizz
* the machine state that the step names changed: each changed value, old and new
* the command is marked `always`
* its task is marked `cache=never`
* `kizz run --no-cache` was given
* the previous run could not be cached, and why

When a step re-runs because of a file that it, or another step, writes on every build, the explanation says so directly,
for example: "re-ran because `~/.cache/go-build/ab/...` changed; this command writes that file and also reads it".
A step that re-runs in consecutive builds for the same self-inflicted reason gets a loud warning,
and the user can decide whether to add an ignore pattern.

`kizz build` prints a one-line reason for each command it runs, and `--explain` prints the full reasons.
`kizz why` prints the reasons stored by the most recent build.
`kizz why` also lists each step's untracked `/proc` and `/sys` accesses, so the user can decide whether any should be tracked.
It also lists each step's connections to the network and to Unix sockets.
It also lists the system calls that a step made and that the tracer does not know, when there are any.

## Order of execution

* Within a task, commands run in program order.
* Tasks run one at a time until everything works: `--sequential` is the default,
  and `--parallel` exits with an error until parallel execution exists.
* By default, parent directories come before their subdirectories, sibling directories come in sorted order,
  and task blocks come in file order.
* After each build, kizz records which tasks consumed paths that another task wrote, including directories another task wrote into.
  A task writes a path through its commands and through its program's `fs.write` and `fs.write_json`.
  The next build runs those producers before their consumers and keeps the default order where there is no such edge.
* Every pass tries every selected task; a failed task does not stop the pass.
* If any command ran during a pass, or a task program changed a file with `fs.write` or `fs.write_json`,
  kizz checks every selected task again in another pass.
  A task whose recorded inputs changed runs again: one whose inputs a later task wrote,
  or one that failed and whose looked-for dependencies have since appeared.
  A task whose inputs did not change is a cache hit, and a failed one fails again from the cache without running.
  A pass runs nothing when no command ran in it and no task program changed a file.
  A `run()` that fails without a record before kizz asks the tracer to start its command is not a command that ran.
  The build ends when a pass runs nothing; it succeeds if no task failed in that pass, and fails otherwise.
  A build settles in the passes up to and including the last one that ran a command or changed a file, or in one pass if none did.
  After such a pass, the final pass, which runs nothing, only confirms the build and is not counted.
  A first build may take extra passes to settle; later builds usually settle in one, because the order has been learned.
  A command marked `always` runs at most once per build, when a pass first reaches it; later passes reuse its result.
* If the build has not settled after a fixed number of passes (one more than the number of tasks),
  kizz stops and names the paths that keep changing and who writes them.
* A missing file is a dependency, never a scheduling signal: kizz never pauses a process to wait for a file to appear.

## Failure and interruption

There is no fail-fast and no `--keep-going`: a build tries everything.
This is the first version of the rule, to be refined later.

* A task fails when its program calls `fail()`, raises an error, calls `run()` with `expect` and gets an unexpected exit,
  or runs a command whose program cannot be found or started.
  With `--fail-unchecked`, a task also fails when it leaves a non-zero exit unchecked.
  Kizz prints the error, the Starlark stack and the captured output of the failing command.
* A failed task does not stop the build:
  the pass continues with every other task, including tasks that may consume its outputs.
* Kizz cannot tell why a task failed.
  If any of its commands looked for a path and did not find it (`ENOENT`), the task may have failed because that path was missing,
  or it may be failing for another reason.
  Kizz does not guess: when a path a failed task looked for appears, the task runs again, because it is no longer missing.
* A failed task whose missing paths are all still missing, and whose other inputs are unchanged, has nothing new to try:
  in later passes it fails again from the cache without running.
  As for any task, a change to anything else it depended on also runs it again.
* A bare program name without `PATH` and an invalid `ignore` or `track` pattern fail their task before kizz starts anything.
  Kizz checks both before the cache check, so every pass of a build fails such a call the same way.
* When a pass runs nothing and failures remain, nothing those tasks looked for has appeared, so kizz stops.
  The build fails with exit status 1 and lists each failed task with the paths it looked for and did not find,
  since any of them may be the cause.
* Steps that finished before a failure keep their cache entries.
* On Ctrl-C, kizz starts nothing new, the running command receives the interrupt, and its result is not cached.
  Kizz kills every process of the command that is still running 2 seconds after the interrupt.
  Kizz then releases and deletes the lock file, closes the database, and exits with status 130.

### Kizz never panics

Kizz never panics: a failure is an error that kizz handles, and a panic is a bug.
The workspace lints forbid code that panics, and `clippy.toml` bans the library calls that panic when the system refuses them.

* Every thread that kizz starts goes through one helper, in `kizz-threads`, which never panics.
* When the system cannot start a thread for the moment (`EAGAIN`), the helper waits and tries again, up to 10 times.
* The waits are 100, 200, 400, 800, 1,600 and 3,200 milliseconds, then 4 seconds four times: 22.3 seconds in all.
* Any other error, or a tenth retry that fails, is an error that names the thread and the system's error.
* A thread that a command's tracer cannot start fails the command's step, and so its task.
* Anywhere else, kizz prints the error and exits with the status of its own errors: 2, or 125 for `kizz execute`.
* A command's parallel work, hashing included, runs on one worker pool of at most 4 threads, never on a global pool.
* SurrealKV, built from kizz's patched copy, never panics when a thread cannot start, as [Database](#database) says.
* Discovery walks on the worker pool, and the store's runtime starts no thread of its own.
* A write to standard output or standard error that fails, such as one to a closed pipe, is dropped, and kizz goes on.
  It fails no step, task or command, and it changes no exit status.
* Every other call that can panic, such as a `RefCell` borrow or `split_at`, uses a form that cannot.
* The workspace lints and `clippy.toml` ban each kind of call that can panic, so that new code cannot bring one back.

## Observing any command with `kizz execute`

`kizz execute` runs one program, traces its whole process tree, and writes an [observation report](#observation-reports).
It needs no `kizz.md`, no repository and no cache.
It shows what an existing build or tool touches, which helps to diagnose it before anything moves to kizz.
It is a diagnostic tool: it never restricts the program, and it never caches anything, so every run executes the program.

* Everything after the required `--` is the program and its arguments, and the program runs without a shell.
* A program name without `/` is looked up in the caller's `PATH`, as `run()` looks one up in the task's.
  Each location that the lookup checks is an observation of the command's first program, as it is a dependency in a build.
  Without `PATH` in the caller's environment, a name without `/` is not found.
* The program gets the caller's whole environment, unchanged, and the caller's working directory.
* It keeps kizz's standard input, output and error, process group and session, and kizz mixes nothing into them.
* Kizz ignores `SIGINT` and `SIGQUIT` while the program runs, as `time` does, and the program gets their default action.
* Kizz forwards a `SIGTERM` or `SIGHUP` that it receives to the program, so one sent to the process group arrives twice.
* A signal that kizz was started with ignored stays ignored, for kizz and for the program.
* Ctrl-Z stops kizz with the program, and `fg` continues both.
  A traced process may still run after Ctrl-Z until its next traced system call or signal, where it waits for kizz.
  A stop signal sent to a traced process alone, such as `kill -STOP`, does not keep it stopped.
* Kizz waits for the whole process tree to end, then replaces `<file>` with the report atomically.
  The program never sees the report, and the directory that holds `<file>` must exist before the program starts.
* The report is written whatever happened: the program exited, failed, was killed or could not start, or tracing failed.
* If kizz itself is killed, the traced processes die with it, and no report is written.
* Kizz never opens the cache, never takes its lock, and never creates a cache directory.

Exit status:

* the program's exit code, or 128 plus the number of the signal that killed it
* 127 when the program is not found, and 126 when it is found but cannot be run or started
* 125 when kizz itself fails after it accepted the command line: the tracer failed, or the report could not be written
* 2 for a command-line error, as for every kizz command, such as a missing `--output` or a missing directory for it

## Running one task with `kizz run`

`kizz run <name>` makes kizz a command runner: it finds the task named `<name>` and runs its program once.

* Discovery is the same as for `kizz build`: the current directory and every directory below it.
* The task whose `kizz.md` is the fewest directory levels below the current directory wins.
* Two tasks of that name at the same level are an error that lists each with its path.
* A name that no task has is an error that lists the task names under the current directory.
* The words after `--` are the task's arguments, which its program reads as `task.args`, and nothing else follows the name.
* Under `kizz run`, `task.selected` is empty, because no target selected the task.
* The task runs once: no other task runs, and no pass follows, even when its program changes a file.
* It takes the cache lock, and it uses and updates the cache as a build does.
* `--no-cache` runs every command of the task, as if each had `always = True`, and still records each result.
* No option forces caching: a command marked `always`, or of a task marked `cache=never`, always runs.
* `--explain`, `--fail-unchecked` and `--cache-unknown-syscalls` work as they do for `kizz build`.
* A command's `stdin` is what its `run()` gives, or `/dev/null`, as in a build.
* Kizz prints each command as it starts, then its output as it comes, as if every `run()` had `stream = True`.
* A cache hit prints its command marked as cached, then replays its stored output.
* `--output=<file>` writes an [observation report](#observation-reports) of the commands of the run.
  `--compact` and `--json` shape it as they do for `kizz execute`.
  Each command that ran is the root of its own process tree, and the task program's own accesses are an entry of their own.
  A cache hit is listed as cached, without observations, so `--no-cache` observes every command.
  `--compact` or `--json` without `--output` is a usage error.
  A report that cannot be written is an error, and a run that otherwise succeeded then exits with 2.
* Unchecked non-zero exits warn after the run, and `--fail-unchecked` fails the task, as in a build.
* Exit status: 0 when the task succeeds, 1 when it fails, 2 for usage and configuration errors, and 130 when interrupted.

## Observation reports

An observation report says what its commands touched, organized by process, for people and for programs.
`kizz execute` writes one for the command it runs, and `kizz run --output` one for the commands of its task.
It aggregates what the tracer saw: each path appears once in its scope, with every access kind and outcome it had.

* Each command has its arguments, its program, its working directory and its outcome.
* The outcome is `exited`, `signaled`, `not_found`, `not_executable`, `start_failed` or `failed`.
  `exited` gives the exit code, `signaled` the signal number, and the others an error message.
  `not_found` and `not_executable` mean a missing or unrunnable program, and `start_failed` another failure to start.
  `failed` means that kizz or its tracer failed, and the report then says that the trace is incomplete.
* A command that came from the cache is listed as cached, without observations.
* The report says whether each command was traced completely, and lists every reason that one was not.
  A reason is a non-native system call, an untraceable call, an unknown system call, a tracer failure or another tracer reason.
  An unknown system call makes the report incomplete, although a build may cache the run with `--cache-unknown-syscalls`.
* Every observation is reported, `/proc` and `/sys` included; ignore patterns and other rules of builds do not apply.
* The totals count the commands, the cached commands, the processes, the programs that are not inherited and the paths.
  They also count the paths of each access kind and the connections.

A report is hierarchical unless `--compact` is given, and both forms come from the same observations.

* A hierarchical report lists each command's processes in the order they started, each under the process that created it.
* Each process has its number, its process ID, its parent's number, its status, and its programs in order.
* Each program has its path, its arguments and whether it is inherited, then its observations and connections.
* A compact report combines the observations of every process of every command into one set, with each path once.
* The combined observations of a hierarchical report are exactly those of the compact report of the same run.
* Paths are sorted, and commands, processes and programs keep their order.
* When kizz started no process for a command, its tree holds one process without a process ID or status.
  Its program is the name that kizz looked up, with the locations the lookup checked.

A report is text unless `--json` is given.

* Text is for reading: a header with the completeness and the totals, then each command and its processes.
  Text groups each program's paths by access kind, and gives each path's outcomes after it.
* JSON is for programs: it names its format, `kizz-execute`, and its version, 1.
  A new field may appear without a new version, but a field that changes its meaning or disappears raises the version.
  JSON is indented by two spaces and ends with a newline.
* Paths and arguments that are not valid UTF-8 appear with each invalid sequence replaced by U+FFFD, in both forms.
* Text escapes each backslash and control character of the strings that it shows, such as paths, arguments and messages.
  Here the control characters are U+0000 through U+001F, U+007F through U+009F, and the separators U+2028 and U+2029.
  A backslash is written as `\\`, a newline as `\n`, a tab as `\t`, and a carriage return as `\r`.
  Any other control character is written as `\u{`, its code in lowercase hexadecimal without leading zeros, and `}`.
  For example, the escape character U+001B is written as `\u{1b}`, and U+0000 as `\u{0}`.
  So no string can break its line, and the escapes never make two different strings read alike.
  JSON keeps its standard escaping, and the escapes of text do not apply to it.

### Observation report fields

The top-level object has these fields, in this order:

* `format`: `kizz-execute`
* `version`: 1
* `mode`: `hierarchical` or `compact`
* `complete`: whether every command that ran was traced completely
* `totals`: the totals, an object
* `commands`: the commands, in the order they ran
* `task_program`: for `kizz run`, the task program's own accesses; otherwise `null`
* `observations`: in a compact report only, every observation of the report
* `connections`: in a compact report only, every connection of the report

`totals` has `commands`, `cached`, `processes`, `programs`, `paths` and `connections`, each a count.
Its `kinds` object counts the paths of each access kind: `read`, `probe`, `list`, `write`, `exec` and `xattr`.

A command has these fields, in this order:

* `argv`: the arguments, starting with the program as given
* `program`: the absolute path of the program, or `null` when the lookup found none
* `cwd`: the working directory
* `cached`: whether the result came from the cache
* `outcome`: an object whose `kind` is the outcome, with `code` for `exited`, `signal` for `signaled`, and `error` otherwise
* `complete`: whether the command was traced completely, or `null` when it came from the cache
* `incomplete`: the reasons that it was not, each an object
* `processes`: in a hierarchical report only, the processes in the order they started

A reason has a `kind`: `non_native_system_call`, `untraceable_system_call`, `unknown_system_call`, `tracer_failed` or `other`.
Its `detail` describes it; `number` is an unknown call's number, and `process` its process in a hierarchical report.

A process has these fields, in this order:

* `id`: its number, unique in the report and counting from 1 across every command
* `pid`: its process ID, or `null` when kizz started no process
* `parent`: the number of the process that created it, or `null`
* `exit`: how it ended, or `null` if the tracer did not see it end
  It is an object whose `kind` is `exited`, with `code`, or `signaled`, with `signal`.
* `programs`: its programs, in order

A program has `path`, `args`, `inherited`, `observations` and `connections`.
An observation has `path` and `access`, an object that maps each access kind of the path to its outcomes.
An outcome is `ok`, an error name such as `ENOENT`, `errno` followed by a number, or `unknown`, and `ok` comes first.
A connection has a `kind`, `ip`, `unix` or `abstract`, and an `address`, a `path` or a `name` to match.
`task_program` has `task`, the task as output names it, and, in a hierarchical report, `observations`.

## Crates and module boundaries

```text
crates/
  kizz/              the binary: command line (bpaf), terminal output (console), banner (build-info)
  kizz-core/         the engine, with no Linux code: discovery, Markdown, Starlark runtime, step keys,
                     cache checks, explanations, ordering, observation reports and running kizz execute
  kizz-store/        the cache location, the lock file, and the SurrealKV store of typed records
  kizz-threads/      the one place that starts threads, waiting and retrying while the system cannot start one
  kizz-trace/        the tracer interface: the run request, the result, the process tree, access kinds,
                     the tracer trait and the platform queries of the core: extended attributes, machine state
                     and error names
  kizz-trace-linux/  the ptrace and seccomp-bpf tracer
```

* `kizz-core` depends on `kizz-trace` and `kizz-store`, never on `kizz-trace-linux`.
* Only the binary wires in the Linux tracer.
* Every kizz crate that starts a thread does so through `kizz-threads`, which depends on no other kizz crate.
* Tests of `kizz-core` use a fake tracer.

## Technology

Kizz is written in Rust, edition 2024, with a minimum Rust version of 1.96, the same as cbork.
The command line and output use the same libraries as the cbork CLI: `bpaf` with derive, `console`, `anyhow` and `build-info`.

| Purpose | Crate | Version when planned |
| --- | --- | --- |
| Command line | `bpaf` with `derive` | 0.9.28 |
| Terminal output | `console` | 0.16.6 |
| Application errors | `anyhow` | 1.0.104 |
| Library error types | `thiserror` | 2.0.21 |
| Version and banner | `build-info`, `build-info-build` | 0.0.47 |
| Hashing | `blake3` with `rayon` and `mmap` | 1.8.2, pinned by `starlark` 0.14.2 |
| Task programs | `starlark` | 0.14.2 |
| Markdown | `pulldown-cmark` | 0.13.5 |
| Database | `surrealkv`, built from kizz's patched copy in `vendor/surrealkv` | 1.0.0 |
| Records and step keys | `minicbor` with `derive` | 2.3.1 |
| JSON | `serde_json` | 1.0.154 |
| Ignore patterns | `globset`, `regex` | 0.4.20, 1.13.1 |
| Concurrency | `rayon` | 1.12.0 |
| In-memory concurrent cache | `dashmap` or similar | to be chosen in phase 5 |
| Linux system calls | `libc` | 0.2.191 |
| Signals | `signal-hook` | 0.4.5 |
| Test fixtures | `tempfile` | 3.27.0 |

File locking uses `std::fs::File::lock`, which is `flock` on Linux.

## Development workflow

* Cargo builds the workspace.
  A Justfile holds the commands, written out directly.
  Moon is not used, because kizz will eventually build itself.
* These settings are copied from cbork:
    * the `[workspace.lints]` tables in `Cargo.toml`: Clippy pedantic as errors; errors for `unwrap`, `expect`, `panic`,
      indexing, unchecked arithmetic and missing documentation, private items included; and the rustdoc lints
    * `clippy.toml`, with its allowances for tests and `msrv = "1.96"`
    * `rustfmt.toml`, which uses nightly rustfmt options
    * `.rumdl.toml`, adding `starlark` to the custom code-block languages
    * `cspell.json` and `.config/dictionaries/project.dic`
    * `deny.toml`, with `AGPL-3.0-only` as the first-party license
* `clippy.toml` also bans the library calls that panic when the system refuses them, such as `std::thread::spawn`.
  Kizz's crates use fallible forms instead, and in them only the helper of `kizz-threads` calls `std::thread::Builder`.
* Source files carry the same copyright and SPDX header as cbork, with `AGPL-3.0-only`.
* Anyone adding spelling words, parallel subagents included, only appends lines to `.config/dictionaries/project.dic`.
  `fix` and `check-ci` sort the file case-insensitively and remove duplicates, so concurrent appends never conflict.
  Case variants collapse into one entry, so add only the canonical form.
* Tests never use the real cache: every test that runs the engine sets `KIZZ_CACHE_DIR` to a temporary directory.
* `Cargo.lock` is committed, unlike in cbork, because kizz is a binary.
* The documentation site is a Zola site in `docs/`, with its own templates and styles and no theme, framework or script.
  `zola --root docs build` builds it into `docs/public/`, which Git ignores, and fails on a broken internal link.
  Its pages link to each other with Zola's `@/` links, so the build checks every one, and external links are not checked.
  The Markdown and spelling checks cover its pages, front matter included.
  Every page has a sidebar that stays in view while the page scrolls, and scrolls on its own when longer than the window.
  It lists each section and page, and each command, Starlark global and member of the two reference pages as a direct link.
  Zola's table of contents of those pages generates their entries, so a new heading appears without a change to the templates.
  The options of each command and the entries of `run()` are in collapsible `<details>` elements that are closed by default.
  The members of `env`, `fs`, `task` and `json` are open by default, so every command, global and member is one click away.
* `vendor/surrealkv` holds SurrealKV 1.0.0 with kizz's patch, which `[patch.crates-io]` uses instead of the published crate.
  The workspace excludes it, and the format, Markdown and spelling checks skip it.
  Only the changes that `vendor/surrealkv/KIZZ-PATCHES.md` lists may differ from the published crate.

The planned Justfile:

```text
set shell := ["bash", "-euo", "pipefail", "-c"]

spelling_dictionary := ".config/dictionaries/project.dic"

# List the recipes.
default:
    @just --list

# Sort the spelling dictionary case-insensitively and remove duplicate words.
sort-dictionary:
    LC_ALL=C sort --ignore-case --unique --output {{spelling_dictionary}} {{spelling_dictionary}}

# Run every automated fixer: formatters and anything else that can be fixed automatically.
fix: sort-dictionary
    cargo +nightly fmt --all
    rumdl check --fix .

# Check formatting, lints, tests, docs, Markdown, spelling and the documentation site. Changes no tracked file.
check:
    cargo +nightly fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace --all-features
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
    rumdl check .
    cspell lint --config cspell.json --no-progress .
    zola --root docs build

# CI: sort the dictionary, run the checks, check licenses, then build.
check-ci: sort-dictionary
    cargo +nightly fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace --all-features
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
    rumdl check .
    cspell lint --config cspell.json --no-progress .
    zola --root docs build
    cargo deny check advisories || true
    cargo deny check bans licenses sources
    cargo build --workspace --release

# Run every fixer, then the CI gate. This is also the quality gate of the workspace agent workflow.
fix-ci: fix check-ci
```

* `fix` fixes: it runs every automated fixer, the formatters and anything else that can be fixed automatically.
* `check` checks and changes no tracked file; it builds the documentation site into the ignored `docs/public/`.
* `check-ci` checks and builds: it sorts the dictionary, as decided, runs the checks, checks licenses,
  and builds the release binaries.
  No recipe passes `--locked`, so Cargo creates and refreshes `Cargo.lock` as needed.
  `cargo deny check advisories` reports the advisories that apply to the dependencies, for information only.
  No finding of that check, of any kind, ever fails `check-ci` or `fix-ci`, and `deny.toml` ignores no advisory.
  `cargo deny check bans licenses sources` comes next, and any finding of those three checks fails the recipe.
* `fix-ci` runs `fix` and then `check-ci`.
  It is also the quality gate the workspace agent workflow runs.

## Implementation plan

Each phase ends with `just fix-ci` passing in the task worktree.
Phases 1, 2 and 3 depend only on phase 0, so separate implementers can build them in parallel.
Phase 8 depends only on phase 5, so it can be built alongside phases 6 and 7.

### Phase 0: workspace scaffold

* The Cargo workspace with the five crates, the lint and formatting settings copied from cbork, and the Justfile.
* The `rumdl`, `cspell` and `cargo-deny` configuration, with `README.md` and this plan passing the Markdown and spelling checks.
* The `kizz` binary with every subcommand parsed; every subcommand prints a scaffold message in the cbork stub style.
* The complete tracer interface in `kizz-trace`: the run request, the result, access kinds, errors and the tracer trait.
* `kizz build` accepts `--sequential`, the default, and rejects `--parallel`
  with an error saying that parallel execution is not implemented yet.

Acceptance: `kizz --help` lists every subcommand, each subcommand prints its stub message,
`kizz build --parallel` fails with the not-implemented error, and `just fix-ci` passes.

### Phase 1: Linux tracer

Depends on phase 0.

* Process start with `ptrace`, `PR_SET_NO_NEW_PRIVS` and the seccomp-bpf filter,
  with tracer options that follow forks, clones and `exec` and kill the traced processes if kizz dies.
* Decoding of the tracked system calls, resolution of relative paths, and reading results at system-call exit.
* `ENOSYS` for `io_uring_setup`, and incomplete-trace reporting for non-native system-call interfaces.
* Feeding `stdin`, split and merged capture, live echo of output, and cancellation.

Acceptance, with real processes:

* `cat` reports a read, `test -e` on a missing path reports a probe, and `ls` reports a directory listing.
* A shell script that creates, renames and removes files reports each write.
* Commands run by child processes and by threads are traced.
* Relative paths resolve correctly after `cd` and through directory descriptors.
* Merged capture keeps the exact order of interleaved writes.
* Exit codes and signals are reported correctly, and cancellation stops the whole process tree.

### Phase 2: cache store and lock

Depends on phase 0.

* The cache location from `KIZZ_CACHE_DIR`, `XDG_CACHE_HOME` and `HOME`.
* The lock file: exclusive `flock`, the holder's details, the waiting message and its updates, deletion on exit,
  takeover after a crash, and the inode check.
* The SurrealKV store with typed CBOR records encoded with `minicbor`,
  prefix scans, multi-record transactions and a schema version.

Acceptance:

* A second process waits and prints the first process's details.
* After the holder is killed with `SIGKILL`, the next build takes the lock at once.
* The lock file is gone after a clean exit.
* Records survive closing and reopening the store, including after a crash.

### Phase 3: discovery and tasks

Depends on phase 0.

* Concurrent discovery of `kizz.md` files from the current directory, with no git filtering,
  following symbolic links to directories except those that would recurse.
* Parsing of task blocks: targets, the nearest heading, line numbers, and the warning for untagged `starlark` blocks.
* Target expressions joined with `+`, and task selection.
* `kizz list`.

Acceptance: fixture trees covering hidden directories, `kizz.md` files that git ignores
(found and used),
symbolic links to directories, including one that would recurse (skipped with a warning) and a `kizz.md` reachable through two paths
(run once), backtick and tilde fences, nested fences, untagged blocks, invalid target names, and a stable order.

### Phase 4: Starlark runtime

Depends on phase 3.

* The dialect and the globals: `run`, `env`, `fs`, `task`, `load`, `print` and `fail`.
* The task environment rules and the dotenv parser.
* File and JSON operations, with every access recorded against the task.
* Module loading, with the task program hash covering every loaded module.
* The boundary to the engine: a step executor that the runtime calls for each `run()`.

Acceptance, with a fake step executor:

* The environment starts empty, and `inherit`, `load_file` with defaults, `set`, `unset` and `clear` behave as specified.
* `expect` fails the task only on unexpected exit codes; without it, `run()` always returns.
* Captured output passed through arguments, `stdin` or the environment reaches the next step's specification.
* JSON round-trips, and a write with unchanged content leaves the file untouched.
* Editing a loaded module changes the task program hash.
* Calling `run()` while a module loads is an error.

### Phase 5: engine and cache

Depends on phases 1, 2 and 4.

* Step keys: the deterministic `minicbor` encoding and the BLAKE3 key derivation.
* Program lookup through the task's `PATH`, recording every location checked.
* Recording path states after each step, concurrently, applying the user's ignore patterns.
* Cache checks with the size, metadata and hash rules, the two-second rule, and metadata refresh.
* Replaying the exit status and output on a hit, `always`, and the results that are never cached.
* The in-memory concurrent cache of file states.
* The `/proc` and `/sys` rules with `track`, and metadata-only records for files that were not read.

Acceptance, with a fake tracer and real files:

* An unchanged step is a cache hit.
* A same-size content change is caught by hashing, and a size change is caught without hashing.
* A touched file that was read, with unchanged content, is a cache hit, and its new metadata is stored.
* A new entry in a listed directory, or a file appearing where a lookup found nothing, re-runs the step.
* Changes to the environment, the arguments, the task block or a loaded module re-run the step.
* An `always` step that rewrites identical content leaves the steps that depend on it cached.
* A file that was only checked is never hashed, and a change to its metadata re-runs the step.
* An access under `/proc` or `/sys` is recorded and shown by `kizz why`, but never causes a re-run unless it is tracked.

### Phase 6: explanations

Depends on phase 5.

* Every reason kind, comparison of step specifications through the step-position records, and last-writer attribution.
* The warning for steps that keep re-running for a self-inflicted reason.
* `kizz why`, the one-line reasons in `kizz build`, and `--explain`.

Acceptance: golden-output tests for every reason kind, including a step that writes a file and also reads it.

### Phase 7: the build command

Depends on phase 6.

* Taking the lock, discovery, the default and learned order, passes until nothing runs, and the non-convergence error.
* Failure handling: every task is tried, and failed tasks run again when a dependency they looked for appears.
* Ctrl-C handling and the build summary.

Acceptance, end to end with the real tracer and a temporary cache:

* Two tasks in the wrong order settle in two passes on the first build and in one pass on the second.
* Building in a subdirectory and then in a parent directory reuses the subdirectory's results.
* A second build waits for the first and reports what it is waiting for.
* An interrupted build leaves no lock file and no cache entry for the interrupted command.
* A task that fails because a file it looks for does not exist yet runs again, and succeeds, once a later task creates it.
* A task whose program a later task builds runs again once the program exists.
* A task that keeps failing with unchanged inputs runs once, then fails from the cache,
  and the build stops with exit status 1 after a pass that runs nothing.

### Phase 8: cache cleanup

Depends on phase 5.

* `kizz cache clean`: a concurrent scan of every recorded path, and removal of the entries described in
  [Cleaning up the cache](#cleaning-up-the-cache).
* `kizz cache reset [path]` and `kizz cache reset --all`, with a confirmation that shows what will be removed,
  `--yes` to skip it, and a refusal to run without a terminal unless `--yes` is given.
* The cache lock for every cache command.

Acceptance, with a temporary cache:

* After a project directory is deleted, `kizz cache clean` removes its entries and keeps every other project's entries.
* A step that depends on a file that has since been deleted is removed,
  and a step that depends on a file that never existed is kept.
* `kizz cache reset` in a directory removes only the entries for tasks under it, and only after confirmation.
* Declining the confirmation changes nothing, and `kizz cache reset --all` empties the cache after confirmation.
* `kizz cache clean` never asks for confirmation.
* Without a terminal, a reset refuses to run unless `--yes` is given, and `--yes` skips the confirmation.
* A cache command started during a build waits for the lock and says what it is waiting for.

### Phase 9: measurement and hardening

Depends on phase 7.

* Tracing overhead and cache-check time, measured on real projects such as one of the Go repositories and `tree-sitter-cddl`.
* Discovery time on large trees.
* A decision, based on the measurements, about whether the tracer needs a fast path.

Acceptance: a written report of the measurements that lists any follow-up work.

### Phase 10: tracer conformance test

Depends on phases 7 and 8.

* `tests/tracer-coverage/` is a fixture project.
  It holds `coverage.c`, the inputs that the program uses, an expectation file and a `kizz.md`.
* In the `kizz.md`, a `coverage-build` task compiles `coverage.c` with `cc`.
  A cached `coverage-test` task runs the program.
* `coverage.c` is plain C that makes every tracked system call, in several variants.
  It checks the result of every call, and calls that only some platforms have are compiled only there.
  It also runs `#!` scripts from the fixture, one of them through an interpreter that is a script itself,
  and one whose interpreter is missing.
* The expectation file lists each path under the task directory that the run must record.
  Each entry gives the path's access kinds, its state, the calls that touch it and its platforms.
* An end-to-end test copies the fixture to a temporary directory and runs `kizz build coverage-build` there.
  It then resets the copy's cache entries with `kizz cache reset` and runs `kizz build coverage-test`.
  It compares the step that `coverage-test` recorded with the expectation file.
* A `coverage-changes` task runs the program in modes that read or run a file and then rewrite it,
  one for each way in which a command first reads a file, and in a mode that writes a file before it reads it.
* A `coverage-incomplete` task runs the program once for each system call that marks a run as incompletely traced,
  and once for each of two numbers that the tracer does not know, 400 and 1,000,000.
* `coverage.c` also binds, connects and sends to Unix sockets, and connects and sends to loopback addresses.
* Another test runs the program through the Linux tracer with the process tree, without a cache.
* A test runs the program with `kizz execute` too, and compares both of its reports with the expectation file.

Acceptance, end to end with the real tracer and a temporary cache:

* The program reports that every call returned what it expected.
* Each expected path for the platform is recorded with exactly its access kinds and state.
* The loader that the program's `PT_INTERP` header names is recorded with the exec kind.
* No other path under the task directory is recorded.
* Each tracked system call has an expected entry that a tracer missing that call would record differently.
* Each step of `coverage-changes` that rewrites a file it read is not cached, and the reason names the file.
* The step that writes its file before it reads it is cached.
* Each step of `coverage-incomplete` is not cached, with a reason that names its system call or number.
* With `kizz build --cache-unknown-syscalls`, the steps of the two numbers are cached and their records list them.
* Each expected connection for the platform is recorded, and no other connection.
* The step's record holds every part of the machine state, with the kernel release that the test reads itself.
* The process tree combines to exactly the run's accesses, holds each child under its parent, and no process for a thread.
* For each expected path, the tree records exactly the outcomes that the entry's sources give each of its kinds.
* Each `coverage-incomplete` call and unknown number appears in the tree as a reason of the program that made it.
* `kizz execute --compact --json` of the program records exactly the expected paths, kinds and outcomes under the task directory.
* Its hierarchical report combines to exactly its compact report.
* For each `coverage-incomplete` call and number, the report of `kizz execute` says that the trace is incomplete and names it.

## Testing

* Each crate has unit tests.
* `kizz-trace-linux` tests trace real processes, so they need `ptrace` and seccomp to be allowed where the tests run.
* `kizz-core` tests use a fake tracer that performs scripted file operations and reports them.
* A fake run that a test builds a report from has every reason, unknown system call and connection of the run in its tree.
* The tracer's bookkeeping of processes has unit tests that replay forks, threads, `exec` calls, exits and reused IDs.
* `kizz-trace-linux` tests run commands in both ways and check their process group, streams and forwarded signals.
* Tests repeat an `openat2` with `RESOLVE_IN_ROOT` or `RESOLVE_BENEATH` at once while it fails with `EAGAIN`, up to 100 attempts.
* End-to-end tests run the `kizz` binary on fixture trees with `KIZZ_CACHE_DIR` set to a temporary directory.
* Tests make thread starts fail on purpose, through a test switch that release builds lack, to check the retries and errors.
* Explanations have golden-output tests.
* A documentation test requires a reference entry for every Starlark global and every member of `env`, `fs` and `task`.
 It also requires one for every `run()` parameter and result attribute, task option, and environment variable that kizz reads.
 It also requires one for every command and option of the command line, and for every field of the observation report.
 It reads these lists from the code, so a new item without an entry fails it.
* A site test builds the documentation site with `zola` into a temporary directory and reads the sidebar of every page.
  It requires a visible link to each command, Starlark global and member that the documentation test reads from the code.
  It also requires a link to every command option and `run()` parameter, inside a group that is closed by default.
  A copy of the site with an added heading shows that a new heading appears in the sidebar without a change to the templates.
  Like the gate's site build, it needs `zola` where the tests run.
* The tracer conformance test of phase 10 needs a C compiler, `cc`, where the tests run.
  Every tracer must pass it: the x86_64 Linux tracer now, and any later one, for aarch64, macOS or Windows.
  Calls that only some platforms have are compiled only there, and their expected entries name those platforms.
  Calls that a kernel may lack because a newer Linux added them are checked only where they are available.
  They are `file_getattr`, `file_setattr`, the `xattrat` calls, `open_tree_attr` and `fchroot`.
  The program first makes each one with arguments that cannot succeed, and a call that returns `ENOSYS` is unavailable.
  `ENOSYS` means that the kernel lacks the call, or that a seccomp filter installed before kizz's own refuses it.
  Their expected entries name the call they need, and where it is available they are required like every other entry.
  Where it is unavailable, nothing may be recorded at their paths, and the test prints a notice that names the call.
  A `coverage-incomplete` step whose call is unavailable need not be marked as incompletely traced.
  The `kizz-trace-linux` tests check these calls the same way; every other call is required on every kernel.
  A seccomp filter installed before kizz's own can refuse a call before the tracer sees it, and then the test of that call fails.
  If that call marks a run or is unknown, `KIZZ_TEST_SKIP_BLOCKED_SYSCALLS=1` makes the test skip it with a notice.
  On Linux the tests also need a temporary directory on a file system with `user.` extended attributes and file handles.

## Open questions

None at the moment.
New *Proposed* items will be listed here with the phase that needs the answer.

## Prior art

Kizz belongs to a known family of tracing build systems.
These are the closest:

* [Riker](https://www.usenix.org/conference/atc22/presentation/curtsinger) traces plain build scripts with `ptrace` and seccomp-BPF.
  Its tracer design and system-call coverage are the reference for phase 1.
* [Rattle](https://arxiv.org/abs/2007.12737) runs traced commands speculatively and detects ordering hazards.
  It informs the learned order and the passes.
* [BuildXL's two-phase cache lookup](https://github.com/microsoft/BuildXL/blob/master/Documentation/Wiki/Advanced-Features/Two-Phase-Cache-Lookup.md)
  matches the cache check here: a key built from what is known before running, followed by the recorded inputs.
