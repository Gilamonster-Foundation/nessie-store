# nessie-store — common operations.
#
# Run `just` (no args) to list recipes. Aligned with the workspace conventions.
#
# PIPELINE PARITY: `ci-check` (and `check`) run the same fmt/clippy/test steps as
# .github/workflows/ci.yml and .githooks/pre-push. When editing the CI pipeline,
# update these recipes AND the hook to match (workspace rule: Push Hook Governance).

# Default: list available recipes.
default:
    @just --list

# Full lock-step check that mirrors CI (run before pushing). `check` formats in
# place; `ci-check` asserts formatting (exact CI parity, no in-place changes).
check: rust-check py-check
    @echo "✓ all checks passed"

# Exact CI/hook parity — fails on unformatted code rather than reformatting.
ci-check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
    cargo test --workspace --locked
    @just py-check

# Rust side — fmt + clippy + tests on the workspace.
rust-check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
    cargo test --workspace --locked

# Python lint across all crate bindings (fast: ruff + black). Wheel build +
# pytest is the heavier `just py-test` (and the CI test-py job).
py-check:
    #!/usr/bin/env bash
    set -euo pipefail
    shopt -s nullglob
    found=0
    for p in crates/*/python crates/*/examples; do
      [ -d "$p" ] || continue
      ruff check "$p"
      black --check "$p"
      found=1
    done
    [ "$found" = "1" ] || echo "(no crate python sources yet, skipping py-check)"

# Build each crate's PyO3 wheel, install into the active venv, run its tests.
# Heavy (compiles every wheel); not part of the pre-push hook — see ci.yml test-py.
py-test:
    #!/usr/bin/env bash
    set -euo pipefail
    shopt -s nullglob
    root="$PWD"
    rm -rf "$root/wheels" && mkdir -p "$root/wheels"
    built=0
    for pp in crates/*/pyproject.toml; do
      d=$(dirname "$pp")
      ( cd "$d" && maturin build --release --out "$root/wheels" )
      built=1
    done
    [ "$built" = "1" ] || { echo "(no crate wheels yet)"; exit 0; }
    pip install --force-reinstall "$root"/wheels/*.whl
    for t in crates/*/python/tests; do
      [ -d "$t" ] && pytest -q "$t"
    done

# Release build of the workspace binaries.
build:
    cargo build --workspace --release --locked

# Build the Python wheel via maturin (drops into wheels/).
maturin:
    maturin build --release --out wheels

# Develop-install the Python module into the current venv for fast iteration.
maturin-develop:
    maturin develop

# Format everything (Rust + Python).
fmt:
    cargo fmt
    @if [ -d python ]; then black python ; fi
    @if [ -d python ]; then ruff check --fix python ; fi

# Install pre-push hooks (mirror CI gates locally).
install-hooks:
    git config core.hooksPath .githooks
    @echo "✓ hooks installed (run \`just check\` to verify gates)"

# Clean rust + python build artifacts.
clean:
    cargo clean
    rm -rf wheels/ target/ python/**/__pycache__ .pytest_cache .mypy_cache .ruff_cache
