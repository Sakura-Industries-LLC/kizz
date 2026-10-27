set shell := ["bash", "-euo", "pipefail", "-c"]
set dotenv-load

spelling_dictionary := ".config/dictionaries/project.dic"

# List the recipes.
default:
    @just --list

# Sort the spelling dictionary case-insensitively and remove duplicate words.
sort-dictionary:
    LC_ALL=C sort --ignore-case --unique --output {{spelling_dictionary}} {{spelling_dictionary}}

# Run every automated fixer: formatters and anything else that can be fixed automatically.
fix: sort-dictionary
    cargo +nightly fmt --all
    rumdl check --fix .

# Check formatting, lints, tests, docs, Markdown, spelling and the documentation site. Changes no tracked file.
check:
    cargo +nightly fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace --all-features
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
    rumdl check .
    cspell lint --config cspell.json --no-progress .
    zola --root docs build

# CI: sort the dictionary, run the checks, check licenses, then build.
check-ci: sort-dictionary
    cargo +nightly fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace --all-features
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
    rumdl check .
    cspell lint --config cspell.json --no-progress .
    zola --root docs build
    cargo deny check advisories || true
    cargo deny check bans licenses sources
    cargo build --workspace --release

# Run every fixer, then the CI gate. This is also the quality gate of the workspace agent workflow.
fix-ci: fix check-ci
