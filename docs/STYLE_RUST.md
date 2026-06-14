# Rust style guide — monty-tui

Aligned with the gilamonster ecosystem conventions. Diverge only with cause; if you find yourself diverging, write down the cause.

## Toolchain

- **Pinned via `rust-toolchain.toml`** — single source of truth at the repo root, shared with downstream consumers.
- **`cargo fmt --check`** must pass. No `rustfmt.toml` overrides without a comment explaining why.
- **`cargo clippy -- -D warnings`** must pass. Zero warnings on `main`.
- **`cargo test --locked`** must pass. The lockfile is committed and authoritative; PRs that change it without a `Cargo.toml` edit get bounced.

## Naming

- Crate names: `monty-*` (the binary), `monty-events`, `monty-sources`, … all lowercase, hyphenated.
- Module names: snake_case. Avoid stuttering (`monty_tui::tui::TuiApp` → `monty_tui::app::App`).
- Types: PascalCase. No prefix soup (`TUiApp` is wrong; `App` is right when the module context already says "tui").
- Errors: one `Error` enum per crate, `thiserror::Error` derived. Variants are named for the *failure mode*, not the *call site* (`Error::NatsConnect`, not `Error::FailedInOpenConnection`).
- Constants: `SCREAMING_SNAKE_CASE`.

## Errors

- `Result<T, Error>` everywhere user code can fail. Library crates use a typed `Error` enum; the binary may use `anyhow::Result` at its outermost edges (main, command handlers) for ergonomics.
- **Never `.unwrap()` or `.expect()` outside tests** unless the invariant is genuinely unreachable — and in that case write `unreachable!("why this is unreachable")` or `expect("WHY this can't be None")`. Bare `.unwrap()` is a code-review block.
- `?` propagation is the default. If you find yourself `match`ing on `Result` just to log and re-return, you're missing a `with_context()` or a `tracing::error!` call upstream.
- Reading external state (NATS, Prometheus, file) — translate the foreign error into your domain error with `From` impls; don't leak `reqwest::Error` through public APIs.

## Async

- `tokio` runtime. `tokio::main` only at the binary edge.
- Library crates take an `impl AsRef<...>` or accept channels rather than spawning background tasks themselves. The binary owns the runtime; libraries describe work.
- `tracing` over `log`. Every span gets a meaningful name, instrumented with `#[instrument(skip(self, large_arg))]`.
- No `block_on` inside an async context. If you need to bridge sync code into async, use `spawn_blocking`.

## Concurrency

- Prefer `Arc<Mutex<T>>` only when actual mutable shared state is required. If the question is "how do I share read-only data," the answer is `Arc<T>`. If the question is "how do I send messages between tasks," the answer is `tokio::sync::mpsc`.
- Avoid `parking_lot` unless you need a specific guarantee `std::sync::Mutex` doesn't give; consistency with std + tokio across the workspace beats microbench wins.

## Dependencies

- Add a dep only when its absence makes the code measurably worse. "Convenience" is not a reason; "this is otherwise 200 lines of subtle bit-twiddling" is.
- Pin to the minor version (`serde = "1.0"`, not `"1.0.196"`). Cargo.lock handles exact reproducibility.
- Prefer crates already in the ecosystem (`tokio`, `serde`, `tracing`, `clap`, `thiserror`, `anyhow`, `ratatui`, `crossterm`) over novel alternatives unless the alternative is the standard in its domain.

## Testing

- **TDD discipline.** New features land with tests. Bug fixes land with a regression test that fails before the fix.
- Unit tests next to the code: `#[cfg(test)] mod tests { ... }`. Integration tests under `tests/`. Property-based tests under `tests/proptest/` when applicable.
- No `tokio::test` without `#[traced_test]` (or the equivalent capture) — if your test fails, the trace output is the only artifact that explains why.
- Mocks: prefer `mockall` for traits, or hand-rolled fakes for narrow surfaces. Don't depend on test-time HTTP servers (`wiremock` etc.) unless integration coverage genuinely needs the wire.

## Public surface

- `pub` is a *promise*. Mark items `pub(crate)` until something outside the crate actually needs them.
- Every `pub fn`, `pub struct`, and `pub enum` gets a doc comment. The first sentence is a one-line summary; the rest can be a paragraph if needed.
- `#[deny(missing_docs)]` is set on library crates. Binaries can be looser, but `mod`-level docs help future-you.

## Comments

- Comments explain *why*, not *what*. The code says what.
- A comment that becomes wrong is worse than no comment. If you change behavior, audit the surrounding prose.
- No commented-out code on `main`. Use `git log` if you need history.

## Commits

- One logical change per commit. The PR can have many; the diff hunks should be readable in isolation.
- Title: imperative mood, ≤ 70 chars. ("Fix race in NATS subscriber" — not "fixed a race")
- Body: the *why*, plus how to verify. Reference issues with `#NNN`.
- No `--no-verify`. Push hooks are CI parity, not optional.
