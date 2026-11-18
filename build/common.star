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


def mise_task_env(*names):
    """Use the same mise installation locations for install and lookup."""
    task_env(
        "PATH",
        "HOME",
        "MISE_DATA_DIR",
        "MISE_CACHE_DIR",
        "MISE_STATE_DIR",
        "MISE_CONFIG_DIR",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
        "XDG_CONFIG_HOME",
        *names,
    )


def version_parts(version):
    """Return the numeric components of a stable version, or None."""
    parts = version.split("+")[0].split(".")
    if "-" in version.split("+")[0] or len(parts) != 3 or not all([part.isdigit() for part in parts]):
        return None
    return [int(part) for part in parts]


def cargo_ignores():
    """Ignore Cargo's mutable bookkeeping and compiler intermediates."""
    target_dir = env.get("CARGO_TARGET_DIR", "target")
    patterns = [
        target_dir + suffix
        for suffix in [
            "/.rustc_info.json",
            "/.rustdoc_fingerprint.json",
            "/**/{.cargo-lock,.cargo-artifact-lock,.cargo-build-lock}",
            "/**/{.fingerprint,incremental}",
            "/**/{.fingerprint,incremental}/**",
            "/**/build/*/*/{fingerprint,run}",
            "/**/build/*/*/{fingerprint,run}/**",
            "/**/{debug,release}/*.d",
        ]
    ]
    cargo_home = env.get("CARGO_HOME")
    if not cargo_home and env.get("HOME"):
        cargo_home = env.get("HOME") + "/.cargo"
    if cargo_home:
        patterns.extend([cargo_home + "/.global-cache" + suffix for suffix in ["", "-journal", "-shm", "-wal"]])
    return patterns


def mise_tool(tool):
    """Use a suitable executable on PATH, or resolve the pinned mise install."""
    pin = run("mise", "config", "get", "--file", "mise.toml", "tools." + tool, expect=[0]).stdout.strip()
    minimum = version_parts(pin)
    if minimum == None:
        fail("Expected an exact stable version for {} in mise.toml, got {}".format(tool, pin))

    found = run("sh", "-c", 'command -v "$1"', "sh", tool)
    if found.rc == 0:
        executable = found.stdout.strip()
        version = run(executable, "--version")
        words = version.stdout.split()
        installed = version_parts(words[-1]) if words else None
        if version.rc == 0 and installed != None and installed >= minimum:
            return executable

    return run("mise", "which", tool, "--tool=" + tool + "@" + pin, expect=[0]).stdout.strip()
