# Copyright (c) 2026 Sakura Industries LLC.
#
# SPDX-License-Identifier: AGPL-3.0-only

"""Build the workspace Rust API reference and make it available to Zola."""

load("common.star", "cargo_ignores", "task_env")


def build_rustdoc():
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
    metadata = run("cargo", "metadata", "--no-deps", "--format-version", "1", ignore=cargo_ignores(), expect=[0])
    doc_dir = json.decode(metadata.stdout)["target_directory"] + "/doc"
    run(
        "cargo",
        "doc",
        "--workspace",
        "--no-deps",
        "--all-features",
        env={"RUSTDOCFLAGS": "-D warnings"},
        ignore=cargo_ignores(),
        always=not fs.exists(doc_dir + "/kizz_core/index.html"),
        stream=True,
        expect=[0],
    )
    static_dir = "docs/static/rustdoc"
    if run("readlink", "--", static_dir, expect=[0, 1]).stdout.strip() != doc_dir:
        run("ln", "-sfnT", "--", doc_dir, static_dir, expect=[0])
