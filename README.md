# kizz

[![Release build](https://github.com/Sakura-Industries-LLC/kizz/actions/workflows/release.yml/badge.svg)](https://github.com/Sakura-Industries-LLC/kizz/actions/workflows/release.yml)
[![Latest release](https://img.shields.io/github/v/release/Sakura-Industries-LLC/kizz)](https://github.com/Sakura-Industries-LLC/kizz/releases/latest)
[![Documentation](https://img.shields.io/badge/docs-online-blue)](https://sakura-industries-llc.github.io/kizz/)

Kizz is a build engine for Linux.
It runs small Starlark programs found in `kizz.md` files, traces every command those programs run,
and records what each command touched as that command's dependencies, so nobody writes dependency lists by hand.

## See what a command touches

`kizz execute` runs any command and reports what it touched, without a `kizz.md` or a cache.
It uses Linux `ptrace` and seccomp-BPF to follow the whole process tree and categorize its file accesses.

The following shows running `cat LICENSE` inside a container; `kizz` tracks and categorizes everything neatly.

```text
root@sjbox:/repo# kizz execute --output=kizz.out -- cat LICENSE

                                 Apache License
                           Version 2.0, January 2004
                        http://www.apache.org/licenses/

   TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION
   ...

root@sjbox:/repo# cat kizz.out
kizz observation report, version 1, hierarchical
trace: complete
totals: commands 1, cached 0, processes 1, programs 1, paths 16, connections 0
paths by access: read 3, probe 12, list 0, write 0, exec 2, xattr 0

command 1: cat LICENSE
  program: /usr/bin/cat
  directory: /repo
  outcome: exited with 0
  trace: complete
  process 1, pid 7, exited with 0
    program /usr/bin/cat: cat LICENSE
      read:
        /etc/ld.so.cache: ok
        /lib/x86_64-linux-gnu/libc.so.6: ok
        /repo/LICENSE: ok
      probe:
        /etc/ld.so.preload: ENOENT
        /opt/android-ndk/cat: ENOENT
        /opt/android-sdk/cmdline-tools/latest/bin/cat: ENOENT
        /opt/android-sdk/emulator/cat: ENOENT
        /opt/android-sdk/platform-tools/cat: ENOENT
        /opt/toolchain/bin/cat: ENOENT
        /root/.cargo/bin/cat: ENOENT
        /root/go/cat: ENOENT
        /usr/bin/cat: ok
        /usr/local/bin/cat: ENOENT
        /usr/local/sbin/cat: ENOENT
        /usr/sbin/cat: ENOENT
      exec:
        /lib64/ld-linux-x86-64.so.2: ok
        /usr/bin/cat: ok
```

## Build tasks

```text
kizz build ci
```

The design and the version 1 plan are in [architecture.md](architecture.md).
Read the [published documentation](https://sakura-industries-llc.github.io/kizz/)
or follow the [documentation build](https://github.com/Sakura-Industries-LLC/kizz/actions/workflows/release.yml).
The site source is in [docs](docs), a Zola site; `zola --root docs serve` shows it locally.

## Development

Tool versions are pinned in [mise.toml](mise.toml).
Install [mise](https://mise.jdx.dev/getting-started.html) and run `mise install`.
Kizz resolves the pinned tools when needed.
Use `mise exec -- just fix-ci` for the Justfile workflow.
Ruff formats Starlark modules and, through rumdl, Starlark code blocks in Markdown.
The [kizz build tasks](kizz.md) document the self-hosted build.

The Justfile holds every command:

* `just fix` runs the formatters and every other automated fixer.
* `just check` checks formatting, lints, tests, docs, Markdown, spelling and the documentation site, and changes no tracked file.
* `just check-ci` sorts the spelling dictionary, runs the checks, checks licenses, and builds the release binaries.
* `just fix-ci` runs `fix` and then `check-ci`.

The Justfile loads a `.env` file beside it if there is one; it holds one machine's settings, and Git ignores it.
If a seccomp filter refuses calls before kizz's tracer sees them, `KIZZ_TEST_SKIP_BLOCKED_SYSCALLS=1` there skips their checks.

## Vendored SurrealKV

Kizz vendors [SurrealKV 1.4.0](vendor/surrealkv), the embedded database for its build cache,
to carry patches that prevent panics when the system cannot start a thread.
The patches let its clock run without a background thread, remove thread creation from write pacing,
and fall back to log recovery on the calling thread when worker threads cannot start.
See the [patch details and rationale](vendor/surrealkv/KIZZ-PATCHES.md).
SurrealKV is licensed under [Apache-2.0](vendor/surrealkv/LICENSE).

## License

AGPL-3.0-only; see [LICENSE](LICENSE).
