# Tracer coverage

The fixture project of the tracer conformance test, phase 10 in `architecture.md`.
The end-to-end test `crates/kizz/tests/tracer_coverage.rs` copies this directory to a temporary directory
and builds the program there with `coverage-build`.
It then resets the copy's cache entries, runs `coverage-test`, and compares the step it recorded with `expected.txt`.
Another end-to-end test builds `coverage-changes` in its own copy, after `coverage-build` and a reset.

## Build the program

```starlark coverage-build
env.inherit("PATH")
run("cc", "-std=gnu11", "-Wall", "-Wextra", "-pthread", "-o", "coverage", "coverage.c", expect = [0])
```

## Run the program

The program runs with an empty environment, and its name has a `/`, so kizz looks nothing up.
Its result is cached, and every access it makes is recorded.

```starlark coverage-test
run("./coverage", capture = "merged", expect = [0])
```

## Rewrite what the program read

Each step runs the program in one mode, which reads or runs one committed file under `in/changes` and then rewrites it,
so its result is not cached.
The `control` mode writes its file under `out/changes` before it reads it, so its result is cached.

```starlark coverage-changes
modes = ["open", "openat", "openat2", "fopen", "execve", "execveat", "execv", "interpreter", "control"]
for mode in modes:
    run("./coverage", "change-" + mode, capture = "merged", expect = [0])
```
