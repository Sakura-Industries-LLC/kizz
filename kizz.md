# Build kizz with kizz

Run `./bootstrap.sh` from a checkout to build and install kizz with Cargo.
It needs Rust 1.100 or later, mise, Node.js and npm.
It forwards arguments to `cargo install`, such as `--root "$HOME/.local"`.
The install root defaults to `CARGO_INSTALL_ROOT`, then `CARGO_HOME`, then `$HOME/.cargo`.
The installed binary runs `kizz run install-tools` to install the mise pins and verify that kizz can run a task.
Put the installation's `bin` directory on `PATH` before running these tasks.

`kizz build fix+ci` is the quality gate.
It runs fixes, checks, tests, documentation builds, dependency checks and the release build.
Each task is named so it can also be run directly with `kizz run <name>`.

| Target | Work |
| --- | --- |
| `fix` | Sort the dictionary and fix Rust, Markdown and Starlark formatting. |
| `ci` | Check dictionary order and formatting, run lints and tests, build docs, check dependencies and build release binaries. |
| `test` | Run the workspace tests. |
| `docs` | Build the Rust documentation and Zola site. |
| `build` | Build release binaries. |
| `tools` | Check the cspell, rumdl and Ruff minimum versions. |

Tasks tagged with both `fix` and `ci` use `task.selected` to choose their mode.
It contains only the task's tags that match the requested targets.
`kizz build ci` checks formatting and dictionary order.
`kizz build fix+ci` uses fix mode for those shared tasks and runs the remaining CI checks.
The formatters report failure if they cannot complete their work.

Each task lists the environment variables its operation uses in its own `task_env(...)` call. [build/common.star](build/common.star)
inherits only those names and reads matching overrides from the optional, ignored root `.env`.
Other `.env` entries never enter that task's command environment.
For the listed names, values in `.env` replace inherited shell values; this is intentional.
Sorting and the site build inherit only `PATH`.
Managed tool tasks also read mise installation locations.
Rust formatting also declares the home directories needed by Cargo and its explicitly selected nightly toolchain.
Cargo compilation tasks declare their toolchain and native build settings;
only the test task accepts `TMPDIR` and `KIZZ_TEST_SKIP_BLOCKED_SYSCALLS`.
Proxy settings belong only to network operations.
For example, `KIZZ_TEST_SKIP_BLOCKED_SYSCALLS=1` skips checks of system calls blocked by an outer seccomp filter.
[mise.toml](mise.toml) is the single source of the cargo-deny, cspell, rumdl and Ruff version pins.
Each managed tool task first checks the executable on its `PATH` against the minimum in that file.
If the executable is missing or unsuitable, the task runs the pinned executable resolved by `mise which`.
Markdown tasks put the selected Ruff directory first on rumdl's `PATH`.

Install [mise](https://mise.jdx.dev/getting-started.html) before bootstrapping.
If kizz is already installed, run `kizz run install-tools` or `mise install` to install the pinned tools.
Run `kizz build fix+ci` directly.
You can also install each pinned tool with its named task.
Node.js and npm must be available for mise's cspell backend.
No kizz build target installs tools automatically.

Cargo, nightly rustfmt and Zola must already be installed.
No project Cargo command requires an up-to-date lock file; Cargo may refresh `Cargo.lock`, which is committed.
Commands whose exit status matters use `expect = [0]`; the advisory check deliberately only reports findings.

## Install all pinned tools

`kizz run install-tools` installs every tool in `mise.toml`.
Bootstrap runs this task using the binary it just installed.

```starlark name=install-tools cache=never
load("build/common.star", "mise_task_env")
mise_task_env("HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "NO_PROXY")
run("mise", "install", network=True, stream=True, expect=[0])
```

## Install cspell

`kizz run install-cspell` installs the exact [mise.toml](mise.toml) pin through mise.
It is opt-in, runs on every invocation, and needs network access only when the tool is missing.

```starlark name=install-cspell cache=never
load("build/common.star", "mise_task_env")
mise_task_env("HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "NO_PROXY")
run("mise", "install", "cspell", network=True, stream=True, expect=[0])
```

## Install rumdl

`kizz run install-rumdl` installs the exact [mise.toml](mise.toml) pin through mise.
It is opt-in, runs on every invocation, and needs network access only when the tool is missing.

```starlark name=install-rumdl cache=never
load("build/common.star", "mise_task_env")
mise_task_env("HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "NO_PROXY")
run("mise", "install", "rumdl", network=True, stream=True, expect=[0])
```

## Install ruff

`kizz run install-ruff` installs the exact [mise.toml](mise.toml) pin through mise.
It is opt-in, runs on every invocation, and needs network access only when the tool is missing.

```starlark name=install-ruff cache=never
load("build/common.star", "mise_task_env")
mise_task_env("HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "NO_PROXY")
run("mise", "install", "ruff", network=True, stream=True, expect=[0])
```

## Check cspell version

`kizz run check-cspell` checks the minimum version independently of spelling.
The spelling task repeats this guard so running it directly also checks the tool.

```starlark tools ci name=check-cspell
load("build/common.star", "mise_task_env", "mise_tool")
mise_task_env()
run(mise_tool("cspell"), "--version", expect=[0])
```

## Check rumdl version

`kizz run check-rumdl` checks the minimum version independently of Markdown checks.
The Markdown tasks also check the version before using it.

```starlark tools fix ci name=check-rumdl
load("build/common.star", "mise_task_env", "mise_tool")
mise_task_env()
run(mise_tool("rumdl"), "--version", expect=[0])
```

## Check Ruff version

The standalone Starlark tasks and Markdown tasks also check this minimum before formatting or checking code.

```starlark tools fix ci name=check-ruff
load("build/common.star", "mise_task_env", "mise_tool")
mise_task_env()
run(mise_tool("ruff"), "--version", expect=[0])
```

## Sort the spelling dictionary

Append words to the dictionary.
In fix mode, or with `kizz run sort-dictionary`, this task sorts the dictionary case-insensitively and removes duplicates.
In CI alone, it fails if entries are out of order or duplicated.

```starlark fix ci name=sort-dictionary
load("build/common.star", "task_env")
task_env("PATH")
env.set("LC_ALL", "C")
dictionary = ".config/dictionaries/project.dic"
mode = ["--output", dictionary] if "fix" in task.selected or not task.selected else ["--check"]
run("sort", "--ignore-case", "--unique", mode, dictionary, expect=[0])
```

## Rust formatting

Nightly rustfmt is needed for the repository's formatting options.

```starlark fix ci name=rustfmt
load("build/common.star", "task_env")
task_env("PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME")
mode = [] if "fix" in task.selected else ["--", "--check"]
run("cargo", "+nightly", "fmt", "--all", mode, stream=True, expect=[0])
```

## Starlark formatting

Ruff recursively discovers `*.star` through [ruff.toml](ruff.toml), including `build/`, and excludes `target/` and `vendor/`.
It treats Starlark as Python-compatible syntax and formats it without Python lint rules.

```starlark fix ci name=format-starlark
load("build/common.star", "mise_task_env", "mise_tool")
mise_task_env()
mode = [] if "fix" in task.selected else ["--check"]
run(mise_tool("ruff"), "format", mode, ".", stream=True, expect=[0])
```

## Markdown formatting

[.rumdl.toml](.rumdl.toml) delegates `starlark` blocks to Ruff for both formatting and formatting checks.
The same Ruff configuration formats standalone modules and Markdown code blocks.
Ruff is required; a missing formatter fails the Markdown check.
The Markdown tasks ignore rumdl's disposable `.rumdl_cache/`, which it reads and rewrites during checks.
Source files and configuration remain tracked.

```starlark fix ci name=format-markdown
load("build/common.star", "mise_task_env", "mise_tool")
mise_task_env()
rumdl = mise_tool("rumdl")
ruff = mise_tool("ruff")
mode = ["--fix"] if "fix" in task.selected else []
run(
    rumdl,
    "check",
    mode,
    ".",
    ignore=[".rumdl_cache", ".rumdl_cache/**"],
    stream=True,
    expect=[0],
    env={"PATH": ruff.rpartition("/")[0] + ":" + env.get("PATH")},
)
```

## Lint Rust

The traced Cargo tasks share `cargo_ignores()` for compiler probe caches, build fingerprints, incremental state,
build-script bookkeeping, lock markers and profile dependency files under `CARGO_TARGET_DIR`.
They also ignore Cargo's global cache database under `CARGO_HOME`, which Cargo updates as it uses dependencies.
Source files, manifests, `Cargo.lock`, and final build artifacts remain tracked.

```starlark ci name=clippy
load("build/common.star", "cargo_ignores", "task_env")
task_env(
    "PATH",
    "HOME",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_JOBS",
    "CARGO_NET_OFFLINE",
    "RUSTFLAGS",
    "CC",
    "AR",
    "PKG_CONFIG_PATH",
)
run(
    "cargo",
    "clippy",
    "--workspace",
    "--all-targets",
    "--all-features",
    "--",
    "-D",
    "warnings",
    ignore=cargo_ignores(),
    stream=True,
    expect=[0],
)
```

## Test the workspace

Tests always use temporary kizz caches and explicit child environments.
The tracer and end-to-end tests need permission to use ptrace and seccomp.
They run with `untraced = True` because Linux permits only one ptrace tracer per process.
This command runs once per build and is never reused from another build; no test dependencies are inferred.
Kizz still captures output, checks the exit status, and cancels the command's process group.
The report explicitly records that dependency tracing was disabled.

```starlark test ci name=test
load("build/common.star", "task_env")
task_env(
    "PATH",
    "HOME",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_JOBS",
    "CARGO_NET_OFFLINE",
    "RUSTFLAGS",
    "CC",
    "AR",
    "PKG_CONFIG_PATH",
    "TMPDIR",
    "KIZZ_TEST_SKIP_BLOCKED_SYSCALLS",
)
run("cargo", "test", "--workspace", "--all-features", untraced=True, stream=True, expect=[0])
```

## Build Rust documentation

```starlark docs ci name=rustdoc
load("build/common.star", "cargo_ignores", "task_env")
task_env(
    "PATH",
    "HOME",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_JOBS",
    "CARGO_NET_OFFLINE",
    "RUSTFLAGS",
    "CC",
    "AR",
    "PKG_CONFIG_PATH",
)
env.set("RUSTDOCFLAGS", "-D warnings")
run("cargo", "doc", "--workspace", "--no-deps", "--all-features", ignore=cargo_ignores(), stream=True, expect=[0])
```

## Check spelling

```starlark ci name=spelling
load("build/common.star", "mise_task_env", "mise_tool")
mise_task_env()
run(mise_tool("cspell"), "lint", "--config", "cspell.json", "--no-progress", ".", stream=True, expect=[0])
```

## Build the documentation site

Zola builds the site into the ignored `docs/public/` directory and checks its internal links.

```starlark docs ci name=site
load("build/common.star", "task_env")
task_env("PATH")
run("zola", "--root", "docs", "build", stream=True, expect=[0])
```

## Install cargo-deny

`kizz run install-cargo-deny` installs the pinned version used by the dependency checks.

```starlark name=install-cargo-deny cache=never
load("build/common.star", "mise_task_env")
mise_task_env("HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "NO_PROXY")
run("mise", "install", "cargo-deny", network=True, stream=True, expect=[0])
```

## Report dependency advisories

Advisory findings are informational and do not fail the build.
Always fetch current advisories, since network responses are not tracked file dependencies.

```starlark ci name=advisories
load("build/common.star", "cargo_ignores", "mise_task_env", "mise_tool")
mise_task_env(
    "CARGO_HOME",
    "CARGO_TARGET_DIR",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
)
result = run(mise_tool("cargo-deny"), "check", "advisories", ignore=cargo_ignores(), always=True, network=True, stream=True)
if result.rc != 0:
    print("Advisory check did not succeed. Continuing with the required checks.")
```

## Check dependency policy

License, source and dependency bans are required checks.

```starlark ci name=licenses
load("build/common.star", "cargo_ignores", "mise_task_env", "mise_tool")
mise_task_env(
    "CARGO_HOME",
    "CARGO_TARGET_DIR",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
)
run(mise_tool("cargo-deny"), "check", "bans", "licenses", "sources", ignore=cargo_ignores(), stream=True, expect=[0])
```

## Build release binaries

Build all workspace release binaries into Cargo's target directory.
`./bootstrap.sh` rebuilds and installs kizz with Cargo; it does not install this existing release artifact.
Because `cargo install` is not locked, bootstrap may resolve newer dependencies than the release task used.

```starlark build ci name=release
load("build/common.star", "cargo_ignores", "task_env")
task_env(
    "PATH",
    "HOME",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_JOBS",
    "CARGO_NET_OFFLINE",
    "RUSTFLAGS",
    "CC",
    "AR",
    "PKG_CONFIG_PATH",
)
run("cargo", "build", "--workspace", "--release", ignore=cargo_ignores(), stream=True, expect=[0])
```
