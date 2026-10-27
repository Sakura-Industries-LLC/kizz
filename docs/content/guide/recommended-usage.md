+++
title = "Recommended usage"
weight = 5
+++

This page recommends how to lay out tasks and targets in a project, and how to use kizz's options well.

## Group tasks with a few well-known targets

Targets are groups, and a project reads best with a few that everybody knows:

* `ci` for everything that continuous integration builds and checks;
* `check` for fast checks, such as formatting and lints, that people run before they commit;
* `test` for the test suites.

A task can carry several targets, so one block serves every group that needs it:

````markdown
## Lint

```starlark ci check
env.inherit("PATH", "HOME")
run("cargo", "clippy", "--all-targets", "--", "-D", "warnings", expect = [0])
```
````

`kizz build ci+check` builds the union, and runs a task that has both targets once.
Because a build starts in the current directory,
the same target builds one component from its directory or the whole project from the top, and both share the cache.

## Inherit only the variables a task needs

A task starts with an empty environment, and its commands see only what it puts there.
Inherit the variables that the commands need, by name, instead of copying the whole caller environment:

```starlark
env.inherit("PATH", "HOME")
env.load_file(".env", defaults = {"GOFLAGS": "-trimpath"}, required = False)
```

Each command's environment is part of its cache key, so an unrelated variable that changes between shells,
such as one that holds a terminal's name, would run the command again for nothing.
A dotenv file loaded with `env.load_file` is a dependency of the task.
Its `defaults` fill in the variables that the file does not set, and with `required = False`, as here, the file may be missing,
and then only the defaults apply.

## Check every result that matters

A non-zero exit fails nothing by itself, so say what is acceptable:

* `expect = [0]` fails the task on any other exit, and kizz prints the command with its output.
* Without `expect`, read `rc` and decide, for a command whose failure means something else, such as `grep`:

```starlark
found = run("grep", "-q", "TODO", "README.md")
if found.rc == 0:
    fail("README.md still holds a TODO")
```

Once a build settles, kizz warns about each non-zero exit or signal that a program left unchecked in the results that stand.
In CI, `kizz build --fail-unchecked ci` turns those warnings into task failures, so none goes unnoticed.

## Share common steps in modules

Put build sequences that several tasks use into a Starlark module, and load them:

```starlark
load("../build/common.star", "go_build")

env.inherit("PATH", "HOME")
go_build("./cmd/server", output = "dist/server", version = "1.0.0")
```

`build/common.star`:

```starlark
def go_build(package, output, version):
    return run(
        "go", "build", "-o", output,
        "-ldflags", "-X main.version=" + version,
        package,
        expect = [0],
    )
```

The path is relative to the file that loads it.
A module's `run()`, `env` and `fs` calls belong to the task that calls them.
Every module is part of the task program, so editing it runs the commands of every task that loads it again.

## Mark commands with effects outside files as always

A cache hit does not repeat a command, so an upload, a database update, a notification or a download would be skipped.
Mark such a command with `always = True`: it then runs in every build, and its dependencies are still recorded.

```starlark
run("rsync", "-a", "dist/", "server:/srv/app/", always = True, expect = [0])
```

## Ignore the caches of tools

Kizz has no knowledge of tools, so it tracks a tool's own cache like any other files.
When a tool keeps writing a cache that it also reads, its command can run again in every build,
and kizz warns loudly about a step that keeps running again for such a self-inflicted reason.
Then leave that cache out with an `ignore` pattern, kept in a variable that every task can share:

```starlark
GO_CACHE = ["~/.cache/go-build/**"]

run("go", "build", "./...", ignore = GO_CACHE, expect = [0])
```

Ignore only what a tool can rebuild by itself: an ignored path is never a reason to run the command again.

## Use timestamps for archive tools

Tools such as `tar` and `zip` store the timestamps of what they pack.
Give them `timestamps = True`, so that a file with a new time but the same content still rebuilds the archive:

```starlark
run("tar", "-cf", "dist/site.tar", "public", timestamps = True, expect = [0])
```

## Name the tasks that people run by hand

Give a name to each task that people run directly, such as a server, a formatter or a release, and run it with `kizz run`:

````markdown
## Format the sources

```starlark check name=fmt
env.inherit("PATH", "HOME")
run("cargo", "fmt", "--all", expect = [0])
```
````

A task with a name and no targets is one that only `kizz run` runs, never a build.
A name must be unique within its `kizz.md`, and `kizz run` picks the task of that name nearest below the current directory.
Two tasks of that name at that nearest depth are an error, so give such tasks different names.
A task of that name at a deeper level is no error, because the nearer task wins.

## Mark deployments as cache=never

A deployment must happen every time it is asked for, even when nothing it reads has changed.
Mark its task with `cache=never`, give it a name and no target, so that no build deploys by accident:

````markdown
## Deploy

```starlark name=deploy cache=never
env.inherit("PATH", "HOME")
env.load_file("deploy.env")
run("sh", "scripts/deploy.sh", task.args[0] if task.args else "staging", expect = [0])
```
````

`kizz run deploy -- production` then deploys to production.

## Learn a tool's inputs with kizz execute first

Before writing a task for a tool, run the tool under `kizz execute` and read the report:

```sh
kizz execute --compact --output=deps.txt -- make
```

The report lists the files, directories and programs that the tool used, as the tracer saw them,
including the configuration files that it looked for and did not find, the caches it wrote, and the network connections it made.
A report shows no environment variable, because the tracer does not see a program read one,
but its paths can point to the variables that a task needs, such as `HOME` for files under the home directory.
Its paths can also show which caches a task may want to ignore, and its connections can suggest that a command needs `always`.
[Observing a command](@/guide/execute.md) explains the report.

## Use a separate cache for builds that must run at once

Only one command uses a cache at a time, and another one waits for it.
When two builds must run at the same time, such as two CI jobs on one machine, give each its own cache:

```sh
KIZZ_CACHE_DIR=/var/cache/kizz-job-1 kizz build ci
```

`KIZZ_CACHE_DIR` must be an absolute path.

## Use containers for isolation

Kizz is not a sandbox: a task can read and write any file and run any command that its user can.
When a build needs isolation, run kizz inside a container, with the cache in a volume that outlives the container.
A container must allow `ptrace` and seccomp filters, which kizz needs to trace commands.
The cache belongs to one machine and user: kizz refuses a cache whose recorded host name, credentials, umask,
architecture or CPU model differ from the current ones, so keep a container's host name and user stable, or give it its own cache.
