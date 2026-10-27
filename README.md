# kizz

Kizz is a build engine for Linux.
It runs small Starlark programs found in `kizz.md` files, traces every command those programs run,
and records what each command touched as that command's dependencies, so nobody writes dependency lists by hand.

```text
kizz build ci
```

The design and the version 1 plan are in [architecture.md](architecture.md).
The documentation site is in [docs](docs), a Zola site; `zola --root docs serve` shows it locally.

## Development

The Justfile holds every command:

* `just fix` runs the formatters and every other automated fixer.
* `just check` checks formatting, lints, tests, docs, Markdown, spelling and the documentation site, and changes no tracked file.
* `just check-ci` sorts the spelling dictionary, runs the checks, checks licenses, and builds the release binaries.
* `just fix-ci` runs `fix` and then `check-ci`.

The Justfile loads a `.env` file beside it if there is one; it holds one machine's settings, and Git ignores it.
If a seccomp filter refuses calls before kizz's tracer sees them, `KIZZ_TEST_SKIP_BLOCKED_SYSCALLS=1` there skips their checks.

## License

AGPL-3.0-only; see [LICENSE](LICENSE).
