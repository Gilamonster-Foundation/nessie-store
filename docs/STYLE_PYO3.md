# PyO3 / maturin style guide — monty-tui

How the Rust ↔ Python boundary is shaped for crates that need to be reachable from both ecosystems (`monty-events` is the canonical one).

Read [STYLE_RUST.md](STYLE_RUST.md) and [STYLE_PYTHON.md](STYLE_PYTHON.md) first; this doc only covers the cross-cutting concerns.

## Build & distribute

- **Build tool:** `maturin` (not `setuptools-rust`). The `pyproject.toml` declares `requires = ["maturin>=1.4,<2.0"]` and `build-backend = "maturin"`.
- **Two artifacts per release:**
  - `cargo publish` → crates.io (`monty-events`)
  - `maturin publish` (or built via CI and pushed to PyPI) → PyPI (`monty-events`)
- Names match across registries: `monty-events` on crates.io = `monty-events` on PyPI. The Python *module* name is `monty_events` (underscore). PyPI consumers `pip install monty-events` then `import monty_events`.
- **Versioning:** SemVer (e.g. `0.1.0`). The Rust `Cargo.toml` version is the source of truth; the Python package reads from it via maturin's `version_from_cargo` config.

## Crate layout

```
crates/monty-events/
├── Cargo.toml              # [package] name = "monty-events"
│                           # [lib] crate-type = ["cdylib", "rlib"]
├── src/
│   ├── lib.rs              # public Rust API + the #[pymodule] entry
│   ├── event.rs            # core types
│   ├── source.rs           # data-source bindings
│   └── python.rs           # ALL #[pyclass]/#[pyfunction]/#[pymethods] live here
└── python/
    ├── monty_events/
    │   ├── __init__.py     # imports from compiled .so + re-exports
    │   └── __init__.pyi    # type stubs (HAND-WRITTEN, COMMITTED)
    └── tests/
        └── test_smoke.py
```

The Rust API is callable from Rust (`rlib`); the Python bindings are an additional shape (`cdylib`) over the same logic. Don't duplicate logic — bind it.

## Binding discipline

- **One `python.rs` module per crate.** All `#[pyclass]`, `#[pyfunction]`, `#[pymethods]` go there. Pure Rust code in other modules stays free of PyO3 macros so Rust-only consumers don't pay an import cost or compile dep.
- **No `Py<T>` in the Rust core.** The core types are normal Rust (`Event`, `EventStream`, ...); `python.rs` wraps them in `#[pyclass]` newtypes that delegate. This means Rust-only crate consumers never link `python3` and stay portable.
- **Convert at the boundary.** `From<RustType> for PyType` impls live in `python.rs`. Don't sprinkle conversion code through the pure-Rust modules.
- **`#[pymethods]` follow `#[pyclass]` immediately.** Don't split the impl block across files for "discoverability" — the boundary is precious; keep it visible.

## Type mapping

| Rust | PyO3 binding | Python user sees |
|---|---|---|
| `&str` | `PyString` or `&str` (zero-copy when safe) | `str` |
| `String` | `String` (cloned across the boundary) | `str` |
| `Vec<T>` where `T: ToPyObject` | `Vec<T>` or `PyList` | `list[T]` |
| `HashMap<String, V>` | `HashMap<String, V>` or `PyDict` | `dict[str, V]` |
| `Option<T>` | `Option<T>` | `T \| None` |
| `Result<T, E>` where `E: Into<PyErr>` | exception | raises `E` (mapped) |
| `chrono::DateTime` | `pyo3-chrono` types or ISO string | `datetime.datetime` |
| `Duration` | `pyo3` `Duration` or seconds float | `datetime.timedelta` or `float` |
| `Uuid` | string | `str` (avoid uuid module imports for hot paths) |
| Async `Future` | `pyo3-asyncio` future | awaitable, in the configured runtime |

Avoid exposing `Result<Option<T>, E>` shapes to Python — they translate poorly. Pick one nullability layer and document it.

## Error mapping

- One `MontyEventsError` base exception in Python (defined via `pyo3::create_exception!`). All Rust errors map to it or to subclasses.
- The Rust `Error` enum has a single `impl From<Error> for PyErr` that walks the variants and constructs the matching Python exception. **Don't catch and re-raise in the binding** — let the typed error flow through with proper `PyErr` construction.
- Panics across the FFI boundary are unrecoverable. Use `Result` everywhere PyO3-side code can fail. The Rust core may use `unreachable!()` for genuine impossibilities, but `python.rs` must catch them and convert.

## GIL discipline

- **`py.allow_threads(|| { ... })`** around any Rust block that:
  - Performs I/O (NATS, HTTP, filesystem)
  - Holds a lock longer than a few microseconds
  - Does CPU work that the GIL would otherwise serialize
- **Don't `acquire_gil()` inside `allow_threads`.** If you need to call back into Python, structure the code so the Rust-side work completes first, then re-enter.
- Iterator yields, callback invocations, and other places where you hand control back to Python are GIL-acquisition points. They cost time. Batch where possible.

## Async

- We use `pyo3-asyncio` with the **tokio** runtime. The Python side calls `await coro()`; the Rust side returns a `Future` wrapped by `pyo3_asyncio::tokio::future_into_py`.
- One global runtime per process. Don't `Runtime::new()` inside a `#[pyfunction]`; the binding crate sets up the runtime once at module init.
- Cancellation: `tokio::select!` against a cancellation token if the call may be long-lived. Python cancellation translates to Rust `CancellationToken::cancel()`.

## Testing

- **Rust tests** in `crates/monty-events/tests/` exercise the pure-Rust API. No `python3` linkage.
- **Python tests** in `python/tests/` exercise the binding surface. They import `monty_events` (the compiled module) and run Python-flavored tests against it.
- CI builds the Rust crate without PyO3 enabled to *guarantee* the `cdylib` features don't leak into the core. Use a feature gate (`python-bindings = ["pyo3"]`) and turn it off for the non-binding test runs.

## Release flow

```bash
# 1. Bump the Cargo.toml version
sed -i 's/^version = .*/version = "0.5.20260512"/' Cargo.toml
just check                          # everything green
just maturin develop                # local install for smoke test
pytest python/tests                 # python tests against the freshly built wheel
cargo test --features python-bindings  # full Rust tests

# 2. Tag + release artifact
git tag v0.5.20260512
git push --tags

# 3. CI handles `cargo publish` + `maturin publish` from the tag
```

Don't release the Python wheel without a matching `cargo publish` of the same version. Skew between the two surfaces is the single most common source of "works on Rust, breaks on Python" reports.

## Common gotchas

- **`#[pyclass]` defaults to `unsendable`.** If you want Python to send the object across threads, set `#[pyclass(unsendable = false)]` AND prove the wrapped type is `Send + Sync`.
- **`#[pyo3(get, set)]`** generates getters/setters for fields; `#[pyo3(get)]` alone makes them read-only from Python. Decide which fields are mutable from Python explicitly — don't accidentally expose internal state.
- **PyPy support** is a non-goal. If we get a PyPy bug report, we add `pypy` to the GH issue labels and defer. CPython is the target.
- **Wheels for every arch** is hard to maintain manually. CI runs `maturin build --release` in a matrix (`linux-x86_64`, `linux-aarch64`, `macos-x86_64`, `macos-aarch64`, `windows-x86_64`). Anything beyond that is on-demand.
