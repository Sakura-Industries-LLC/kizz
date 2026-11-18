# Copyright (c) 2026 Sakura Industries LLC.
#
# SPDX-License-Identifier: AGPL-3.0-only

"""Explicit task environment overrides and tool version checks for kizz's build."""


def task_env(*names):
    """Inherit only named variables, with matching .env entries taking precedence."""
    env.inherit(*names)
    if fs.exists(".env"):
        overrides = env.read(".env")
        for name in names:
            if name in overrides:
                env.set(name, overrides[name])


def version_parts(version):
    """Parse the three numeric components of a stable semantic version."""
    if "-" in version:
        fail("A stable tool version is required; got " + version)
    parts = version.split("+")[0].split(".")
    if len(parts) != 3:
        fail("Expected a three-part tool version; got " + version)
    return [int(part) for part in parts]


def cargo_ignores():
    """Ignore Cargo's disposable compiler probe cache, not build inputs or outputs."""
    return [env.get("CARGO_TARGET_DIR", "target") + "/.rustc_info.json"]


def require_tool(tool):
    """Use mise.toml as the single source of minimum and installation versions."""
    minimum = run(
        "mise",
        "config",
        "get",
        "--file",
        "mise.toml",
        "tools." + tool,
        expect=[0],
    ).stdout.strip()
    result = run(tool, "--version", expect=[0])
    installed = result.stdout.strip().split()[-1]
    if version_parts(installed) < version_parts(minimum):
        fail(
            "{} {} is too old; need at least {}. Run `mise install {}` and activate mise.".format(
                tool,
                installed,
                minimum,
                tool,
            )
        )
