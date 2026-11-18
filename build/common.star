# Copyright (c) 2026 Sakura Industries LLC.
#
# SPDX-License-Identifier: AGPL-3.0-only

"""Environment and tool version checks for kizz's own build tasks."""

def setup():
    """Use explicit caller settings, then optional machine settings from .env."""
    env.inherit(
        "PATH", "HOME", "TMPDIR", "TERM", "NO_COLOR",
        "CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN", "CARGO_TARGET_DIR",
        "CARGO_BUILD_JOBS", "CARGO_NET_OFFLINE", "RUSTFLAGS", "RUSTDOCFLAGS",
        "CC", "CXX", "AR", "PKG_CONFIG_PATH",
        "HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "NO_PROXY",
        "NPM_CONFIG_PREFIX", "npm_config_prefix",
        "KIZZ_TEST_SKIP_BLOCKED_SYSCALLS",
    )
    env.load_file(".env", required = False)
    return env.read(".config/build.env")

def version_parts(version):
    """Parse the three numeric components of a stable semantic version."""
    if "-" in version:
        fail("A stable tool version is required; got " + version)
    parts = version.split("+")[0].split(".")
    if len(parts) != 3:
        fail("Expected a three-part tool version; got " + version)
    return [int(part) for part in parts]

def require_tool(tool, minimum):
    """Fail before using an old tool, with the command that installs its pin."""
    result = run(tool, "--version", expect = [0])
    installed = result.stdout.strip().split()[-1]
    if version_parts(installed) < version_parts(minimum):
        fail("{} {} is too old; need at least {}. Run `kizz run install-{}`.".format(
            tool, installed, minimum, tool,
        ))
