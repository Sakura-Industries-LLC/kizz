# Tracer coverage

The fixture project of the tracer conformance test, phase 10 in `architecture.md`.
The end-to-end test `crates/kizz/tests/tracer_coverage.rs` copies this directory to a temporary directory
and builds the program there with `coverage-build`.
It then resets the copy's cache entries, runs `coverage-test`, and compares the step it recorded with `expected.txt`.
Another end-to-end test builds `coverage-changes` in its own copy, after `coverage-build` and a reset.
Two more build `coverage-incomplete` in copies of their own, after `coverage-build` and a reset,
one of them with `--cache-unknown-syscalls`.

## Build the program

```starlark coverage-build
env.inherit("PATH")
run("cc", "-std=gnu11", "-Wall", "-Wextra", "-pthread", "-o", "coverage", "coverage.c", expect=[0])
```

## Run the program

The program runs with an empty environment, and its name has a `/`, so kizz looks nothing up.
Its result is cached, and every access it makes is recorded.

```starlark coverage-test
run("./coverage", capture="merged", expect=[0])
```

## Rewrite what the program read

Each step runs the program in one mode, which reads or runs one committed file under `in/changes` and then rewrites it,
so its result is not cached.
The `control` mode writes its file under `out/changes` before it reads it, so its result is cached.

```starlark coverage-changes
modes = ["open", "openat", "openat2", "fopen", "execve", "execveat", "execv", "interpreter", "control"]
for mode in modes:
    run("./coverage", "change-" + mode, capture="merged", expect=[0])
```

## Make calls that kizz cannot trace

Each step runs the program in one mode, which makes only one system call and exits with 0 whatever it returns.
The calls of the first loop mark the run as incompletely traced, so their results are not cached.
Each is made in a way that can change nothing if it succeeds: a missing path, an invalid descriptor or handle,
or flags that the kernel rejects.
The second loop makes system calls 400 and 1,000,000, which the tracer does not know,
so their results are not cached unless `kizz build --cache-unknown-syscalls` is given.

```starlark coverage-incomplete
calls = [
    "open_by_handle_at",
    "chroot",
    "fchroot",
    "pivot_root",
    "mount",
    "umount2",
    "fsopen",
    "fsconfig",
    "fsmount",
    "fspick",
    "move_mount",
    "open_tree",
    "open_tree_attr",
    "mount_setattr",
    "unshare",
    "setns",
    "clone",
    "clone3",
    "uselib",
    "acct",
    "quotactl",
    "quotactl_fd",
    "swapon",
    "swapoff",
]
for call in calls:
    run("./coverage", "incomplete-" + call, capture="merged", expect=[0])
for number in ["400", "1000000"]:
    run("./coverage", "unknown-" + number, capture="merged", expect=[0])
```
