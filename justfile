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

# Python side — ruff + black + mypy + pytest (skipped while no Python yet).
py-check:
    @if [ -d python ]; then \
        ruff check python ; \
        black --check python ; \
        mypy python/nessie_store_client python/tests ; \
        pytest -n auto python/tests ; \
    else \
        echo "(no python/ yet, skipping py-check)" ; \
    fi

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
