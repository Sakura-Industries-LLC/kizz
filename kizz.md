# Build kizz with kizz

Run `./bootstrap.sh` from a checkout to build and install kizz with Cargo.
It needs Rust 1.100 or later and forwards arguments to `cargo install`, such as `--root "$HOME/.local"`.
Put the installation's `bin` directory on `PATH` before running these tasks.
The bootstrap only installs kizz; it does not install the other development tools.

`kizz build fix+ci` runs the same fixes, checks, tests, documentation builds, license checks and release build as `just fix-ci`.
The Justfile remains available.
Each task is named so it can also be run directly with `kizz run <name>`.

| Target | Work |
| --- | --- |
| `fix` | Sort the dictionary and fix Rust, Markdown and Starlark formatting. |
| `check` | Check formatting, lints, tests, spelling and documentation. |
| `ci` | Sort the dictionary, run every check, check dependencies and build release binaries. |
| `test` | Run the workspace tests. |
| `docs` | Build the Rust documentation and Zola site. |
| `build` | Build release binaries. |
| `tools` | Check the cspell, rumdl and Ruff minimum versions. |

Each task lists the environment variables its operation uses in its own `task_env(...)` call. [build/common.star](build/common.star)
inherits only those names and reads matching overrides from the optional, ignored root `.env`.
Other `.env` entries never enter that task's command environment.
For the listed names, values in `.env` replace inherited shell values; this is intentional.
Markdown formatting, spelling, version checks, sorting and the site build inherit only `PATH`.
Rust formatting also declares the home directories needed by Cargo and its explicitly selected nightly toolchain.
Cargo compilation tasks declare their toolchain and native build settings;
only the test task accepts `TMPDIR` and `KIZZ_TEST_SKIP_BLOCKED_SYSCALLS`.
Mise storage settings belong only to installers, and proxy settings belong only to network operations.
The Justfile also loads `.env`, but its default precedence favors the shell instead.
For example, `KIZZ_TEST_SKIP_BLOCKED_SYSCALLS=1` skips checks of system calls blocked by an outer seccomp filter.
[mise.toml](mise.toml) is the single source of the cspell, rumdl and Ruff version pins.
The build checks accept those versions or newer stable versions, including manually installed tools.
Mise installs the exact pins; the checks read them with `mise config get --file mise.toml`.

Install [mise](https://mise.jdx.dev/getting-started.html), then run `mise install`.
Activate mise in your shell, or run `mise exec -- kizz build fix+ci` and `mise exec -- just fix-ci`.
The optional named installer tasks below install one tool at a time.
Node.js and npm must be available for mise's cspell backend.
No kizz build target installs tools automatically.

Cargo, nightly rustfmt, cargo-deny and Zola must already be installed, as with the Justfile.
No project Cargo command requires an up-to-date lock file; Cargo may refresh `Cargo.lock`, which is committed.
Commands whose exit status matters use `expect = [0]`; the advisory check deliberately only reports findings.

## Install cspell

`kizz run install-cspell` installs the exact [mise.toml](mise.toml) pin through mise.
It is opt-in, runs on every invocation, and needs network access only when the tool is missing.

```starlark name=install-cspell cache=never
load("build/common.star", "task_env")
task_env(
    "PATH",
    "HOME",
    "MISE_DATA_DIR",
    "MISE_CACHE_DIR",
    "MISE_STATE_DIR",
    "MISE_CONFIG_DIR",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
)
run("mise", "install", "cspell", stream=True, expect=[0])
```

## Install rumdl

`kizz run install-rumdl` installs the exact [mise.toml](mise.toml) pin through mise.
It is opt-in, runs on every invocation, and needs network access only when the tool is missing.

```starlark name=install-rumdl cache=never
load("build/common.star", "task_env")
task_env(
    "PATH",
    "HOME",
    "MISE_DATA_DIR",
    "MISE_CACHE_DIR",
    "MISE_STATE_DIR",
    "MISE_CONFIG_DIR",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
)
run("mise", "install", "rumdl", stream=True, expect=[0])
```

## Install ruff

`kizz run install-ruff` installs the exact [mise.toml](mise.toml) pin through mise.
It is opt-in, runs on every invocation, and needs network access only when the tool is missing.

```starlark name=install-ruff cache=never
load("build/common.star", "task_env")
task_env(
    "PATH",
    "HOME",
    "MISE_DATA_DIR",
    "MISE_CACHE_DIR",
    "MISE_STATE_DIR",
    "MISE_CONFIG_DIR",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
)
run("mise", "install", "ruff", stream=True, expect=[0])
```

## Check cspell version

`kizz run check-cspell` checks the minimum version independently of spelling.
The spelling task repeats this guard so running it directly also checks the tool.

```starlark tools check ci name=check-cspell
load("build/common.star", "require_tool", "task_env")
task_env("PATH")
require_tool("cspell")
```

## Check rumdl version

`kizz run check-rumdl` checks the minimum version independently of Markdown checks.
The Markdown tasks also check the version before using it.

```starlark tools fix check ci name=check-rumdl
load("build/common.star", "require_tool", "task_env")
task_env("PATH")
require_tool("rumdl")
```

## Check Ruff version

The standalone Starlark tasks and Markdown tasks also check this minimum before formatting or checking code.

```starlark tools fix check ci name=check-ruff
load("build/common.star", "require_tool", "task_env")
task_env("PATH")
require_tool("ruff")
```

## Sort the spelling dictionary

Append words to the dictionary; this task sorts case-insensitively and removes duplicates.
CI includes it, matching `just check-ci`.

```starlark fix ci name=sort-dictionary
load("build/common.star", "task_env")
task_env("PATH")
env.set("LC_ALL", "C")
run(
    "sort",
    "--ignore-case",
    "--unique",
    "--output",
    ".config/dictionaries/project.dic",
    ".config/dictionaries/project.dic",
    expect=[0],
)
```

## Fix Rust formatting

Nightly rustfmt is needed for the repository's formatting options.

```starlark fix name=fix-rustfmt
load("build/common.star", "task_env")
task_env("PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME")
run("cargo", "+nightly", "fmt", "--all", stream=True, expect=[0])
```

## Fix Starlark modules

Ruff recursively discovers `*.star` through [ruff.toml](ruff.toml), including `build/`, and excludes `target/` and `vendor/`.
It treats Starlark as Python-compatible syntax and formats it without Python lint rules.

```starlark fix name=fix-starlark
load("build/common.star", "require_tool", "task_env")
task_env("PATH")
require_tool("ruff")
run("ruff", "format", ".", stream=True, expect=[0])
```

## Fix Markdown

[.rumdl.toml](.rumdl.toml) delegates `starlark` blocks to Ruff for both formatting and formatting checks.
The same Ruff configuration formats standalone modules and Markdown code blocks.
Ruff is required; a missing formatter fails the Markdown check.
The Markdown tasks ignore rumdl's disposable `.rumdl_cache/`, which it reads and rewrites during checks.
Source files and configuration remain tracked.

```starlark fix name=fix-markdown
load("build/common.star", "require_tool", "task_env")
task_env("PATH")
require_tool("rumdl")
require_tool("ruff")
run("rumdl", "check", "--fix", ".", ignore=[".rumdl_cache", ".rumdl_cache/**"], stream=True, expect=[0])
```

## Check Rust formatting

```starlark check ci name=check-rustfmt
load("build/common.star", "task_env")
task_env("PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME")
run("cargo", "+nightly", "fmt", "--all", "--", "--check", stream=True, expect=[0])
```

## Check Starlark modules

```starlark check ci name=check-starlark
load("build/common.star", "require_tool", "task_env")
task_env("PATH")
require_tool("ruff")
run("ruff", "format", "--check", ".", stream=True, expect=[0])
```

## Lint Rust

The Cargo compilation tasks ignore only `.rustc_info.json` in the target directory, respecting `CARGO_TARGET_DIR`.
Clippy and rustdoc rewrite that disposable compiler probe cache with different values;
tracking it makes them repeatedly invalidate each other.
Source files and build artifacts remain tracked.

```starlark check ci name=clippy
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

```starlark test check ci name=test
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

```starlark docs check ci name=rustdoc
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

## Check Markdown

```starlark check ci name=check-markdown
load("build/common.star", "require_tool", "task_env")
task_env("PATH")
require_tool("rumdl")
require_tool("ruff")
run("rumdl", "check", ".", ignore=[".rumdl_cache", ".rumdl_cache/**"], stream=True, expect=[0])
```

## Check spelling

```starlark check ci name=spelling
load("build/common.star", "require_tool", "task_env")
task_env("PATH")
require_tool("cspell")
run("cspell", "lint", "--config", "cspell.json", "--no-progress", ".", stream=True, expect=[0])
```

## Build the documentation site

Zola builds the site into the ignored `docs/public/` directory and checks its internal links.

```starlark docs check ci name=site
load("build/common.star", "task_env")
task_env("PATH")
run("zola", "--root", "docs", "build", stream=True, expect=[0])
```

## Report dependency advisories

As with `just check-ci`, advisory findings are informational and do not fail the build.
Always fetch current advisories, since network responses are not tracked file dependencies.

```starlark ci name=advisories
load("build/common.star", "task_env")
task_env(
    "PATH",
    "HOME",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
)
result = run("cargo", "deny", "check", "advisories", always=True, stream=True)
if result.rc != 0:
    print("Advisory check did not succeed; continuing as just check-ci does.")
```

## Check dependency policy

License, source and dependency bans are required checks.

```starlark ci name=licenses
load("build/common.star", "task_env")
task_env(
    "PATH",
    "HOME",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
)
run("cargo", "deny", "check", "bans", "licenses", "sources", stream=True, expect=[0])
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
