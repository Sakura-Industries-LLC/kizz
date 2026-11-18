# Build kizz with kizz

Run `./bootstrap.sh` from a checkout to build and install kizz with Cargo.
It needs Rust 1.100 or later and forwards arguments to `cargo install`, such as `--root "$HOME/.local"`.
Put the installation's `bin` directory on `PATH` before running these tasks.
The bootstrap only installs kizz; it does not install the other development tools.

`kizz build fix+ci` selects the same fixes, checks, tests, documentation builds, license checks and release build as `just fix-ci`.
The Justfile remains available.
Each task is named so it can also be run directly with `kizz run <name>`.

The test task currently needs an engine change: Linux cannot attach two ptrace tracers to one process,
so the tracer and end-to-end tests fail when kizz traces them.
Until an explicit untraced test mode is available, use `just fix-ci` for the complete quality gate.
The other tasks can be run with kizz already.

| Target | Work |
| --- | --- |
| `fix` | Sort the dictionary and fix Rust and Markdown formatting. |
| `check` | Check formatting, lints, tests, spelling and documentation. |
| `ci` | Sort the dictionary, run every check, check dependencies and build release binaries. |
| `test` | Run the workspace tests. |
| `docs` | Build the Rust documentation and Zola site. |
| `build` | Build release binaries. |
| `tools` | Check the cspell and rumdl minimum versions. |

The tasks use explicit environment variables through [build/common.star](build/common.star) and load the optional,
ignored root `.env` for machine settings, just as the Justfile does.
For example, `KIZZ_TEST_SKIP_BLOCKED_SYSCALLS=1` skips checks of system calls blocked by an outer seccomp filter.
The committed [.config/build.env](.config/build.env) holds the exact cspell and rumdl installation versions;
checks accept these versions or newer stable versions.
Changing a pin updates both its installer and its minimum version check.

Cargo, nightly rustfmt, cargo-deny and Zola must already be installed, as with the Justfile.
No project Cargo command requires an up-to-date lock file; Cargo may refresh `Cargo.lock`, which is committed.
Commands whose exit status matters use `expect = [0]`; the advisory check deliberately only reports findings.

## Install cspell

Run `kizz run install-cspell` to install the exact pin with npm, or install that version yourself.
Node.js and npm must already be available, and npm's global prefix must be writable by your user.
Set `NPM_CONFIG_PREFIX` if you use a custom prefix, and put its `bin` directory on `PATH`.
This task is opt-in and installs on every invocation; no build target installs tools.

```starlark name=install-cspell cache=never
load("build/common.star", "setup")
pins = setup()
run("npm", "install", "--global", "cspell@" + pins["CSPELL_VERSION"], stream = True, expect = [0])
```

## Install rumdl

Run `kizz run install-rumdl` to install the exact pin with Cargo, or install that version yourself.
The binary goes into Cargo's installation directory, normally `$HOME/.cargo/bin`, which must be on `PATH`.
This task is also opt-in and runs on every invocation.
The installer's `--locked` uses rumdl's published dependency lock file, not this project's lock file.

```starlark name=install-rumdl cache=never
load("build/common.star", "setup")
pins = setup()
run("cargo", "install", "rumdl", "--version", "=" + pins["RUMDL_VERSION"], "--locked", stream = True, expect = [0])
```

## Check cspell version

`kizz run check-cspell` checks the minimum version independently of spelling.
The spelling task repeats this guard so running it directly also checks the tool.

```starlark tools check ci name=check-cspell
load("build/common.star", "require_tool", "setup")
pins = setup()
require_tool("cspell", pins["CSPELL_VERSION"])
```

## Check rumdl version

`kizz run check-rumdl` checks the minimum version independently of Markdown checks.
The Markdown tasks also check the version before using it.

```starlark tools fix check ci name=check-rumdl
load("build/common.star", "require_tool", "setup")
pins = setup()
require_tool("rumdl", pins["RUMDL_VERSION"])
```

## Sort the spelling dictionary

Append words to the dictionary; this task sorts case-insensitively and removes duplicates.
CI includes it, matching `just check-ci`.

```starlark fix ci name=sort-dictionary
load("build/common.star", "setup")
setup()
env.set("LC_ALL", "C")
run("sort", "--ignore-case", "--unique", "--output", ".config/dictionaries/project.dic",
    ".config/dictionaries/project.dic", expect = [0])
```

## Fix Rust formatting

Nightly rustfmt is needed for the repository's formatting options.

```starlark fix name=fix-rustfmt
load("build/common.star", "setup")
setup()
run("cargo", "+nightly", "fmt", "--all", stream = True, expect = [0])
```

## Fix Markdown

The Markdown tasks ignore rumdl's disposable `.rumdl_cache/`, which it reads and rewrites during checks.
Source files and configuration remain tracked.

```starlark fix name=fix-markdown
load("build/common.star", "require_tool", "setup")
pins = setup()
require_tool("rumdl", pins["RUMDL_VERSION"])
run("rumdl", "check", "--fix", ".", ignore = [".rumdl_cache", ".rumdl_cache/**"], stream = True, expect = [0])
```

## Check Rust formatting

```starlark check ci name=check-rustfmt
load("build/common.star", "setup")
setup()
run("cargo", "+nightly", "fmt", "--all", "--", "--check", stream = True, expect = [0])
```

## Lint Rust

```starlark check ci name=clippy
load("build/common.star", "setup")
setup()
run("cargo", "clippy", "--workspace", "--all-targets", "--all-features", "--", "-D", "warnings",
    stream = True, expect = [0])
```

## Test the workspace

Tests always use temporary kizz caches and explicit child environments.
The tracer and end-to-end tests need permission to use ptrace and seccomp.

```starlark test check ci name=test
load("build/common.star", "setup")
setup()
run("cargo", "test", "--workspace", "--all-features", stream = True, expect = [0])
```

## Build Rust documentation

```starlark docs check ci name=rustdoc
load("build/common.star", "setup")
setup()
env.set("RUSTDOCFLAGS", "-D warnings")
run("cargo", "doc", "--workspace", "--no-deps", "--all-features", stream = True, expect = [0])
```

## Check Markdown

```starlark check ci name=check-markdown
load("build/common.star", "require_tool", "setup")
pins = setup()
require_tool("rumdl", pins["RUMDL_VERSION"])
run("rumdl", "check", ".", ignore = [".rumdl_cache", ".rumdl_cache/**"], stream = True, expect = [0])
```

## Check spelling

```starlark check ci name=spelling
load("build/common.star", "require_tool", "setup")
pins = setup()
require_tool("cspell", pins["CSPELL_VERSION"])
run("cspell", "lint", "--config", "cspell.json", "--no-progress", ".", stream = True, expect = [0])
```

## Build the documentation site

Zola builds the site into the ignored `docs/public/` directory and checks its internal links.

```starlark docs check ci name=site
load("build/common.star", "setup")
setup()
run("zola", "--root", "docs", "build", stream = True, expect = [0])
```

## Report dependency advisories

As with `just check-ci`, advisory findings are informational and do not fail the build.
Always fetch current advisories, since network responses are not tracked file dependencies.

```starlark ci name=advisories
load("build/common.star", "setup")
setup()
result = run("cargo", "deny", "check", "advisories", always = True, stream = True)
if result.rc != 0:
    print("Advisory check did not succeed; continuing as just check-ci does.")
```

## Check dependency policy

License, source and dependency bans are required checks.

```starlark ci name=licenses
load("build/common.star", "setup")
setup()
run("cargo", "deny", "check", "bans", "licenses", "sources", stream = True, expect = [0])
```

## Build release binaries

Build all workspace release binaries into Cargo's target directory.
To install the resulting kizz, run `./bootstrap.sh` again.

```starlark build ci name=release
load("build/common.star", "setup")
setup()
run("cargo", "build", "--workspace", "--release", stream = True, expect = [0])
```
