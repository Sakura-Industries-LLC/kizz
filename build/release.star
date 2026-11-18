# Copyright (c) 2026 Sakura Industries LLC.
#
# SPDX-License-Identifier: AGPL-3.0-only

"""Manage the Git tag that triggers the release workflow."""

load("common.star", "task_env")


def release_env():
    task_env(
        "PATH",
        "HOME",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "RUSTUP_TOOLCHAIN",
        "XDG_CONFIG_HOME",
        "SSH_AUTH_SOCK",
        "SSH_ASKPASS",
        "GIT_SSH",
        "GIT_SSH_COMMAND",
        "GIT_ASKPASS",
        "GPG_TTY",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
    )


def release_tag():
    metadata = run("cargo", "metadata", "--no-deps", "--format-version", "1", expect=[0])
    for package in json.decode(metadata.stdout)["packages"]:
        if package["name"] == "kizz":
            return "v" + package["version"]
    fail("Cargo metadata did not include the kizz package")


def local_tag_oid(ref):
    result = run("git", "rev-parse", "--verify", "--quiet", ref, expect=[0, 1])
    return result.stdout.strip() if result.rc == 0 else None


def publish_tag():
    release_env()
    tag = release_tag()
    ref = "refs/tags/" + tag
    if run("git", "status", "--porcelain", "--untracked-files=no", expect=[0]).stdout:
        fail("Commit tracked changes before releasing " + tag)

    head = run("git", "rev-parse", "--verify", "HEAD", expect=[0]).stdout.strip()
    if local_tag_oid(ref) == None:
        run("git", "tag", tag, head, stream=True, expect=[0])
    elif run("git", "rev-parse", "--verify", ref + "^{commit}", expect=[0]).stdout.strip() != head:
        fail("Local tag {} points to another commit".format(tag))

    run("git", "push", "--no-follow-tags", "origin", ref + ":" + ref, network=True, stream=True, expect=[0])


def remove_tag():
    release_env()
    tag = release_tag()
    ref = "refs/tags/" + tag
    local = local_tag_oid(ref)
    remote = run("git", "ls-remote", "--refs", "--exit-code", "origin", ref, network=True, expect=[0, 2])
    if remote.rc == 0:
        oid = remote.stdout.partition("\t")[0]
        if local != None and local != oid:
            fail("Local and remote tags differ for " + tag)
        run(
            "git",
            "push",
            "--no-follow-tags",
            "--force-with-lease=" + ref + ":" + oid,
            "origin",
            ":" + ref,
            network=True,
            stream=True,
            expect=[0],
        )
    if local != None:
        run("git", "update-ref", "-d", ref, local, expect=[0])
