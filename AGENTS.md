# Calexandria

A personal agent harness in Rust; the command is `calx`. It ships as a Python
wheel built with maturin (`bindings = "bin"`), because the owner's work machine
only allows software installed as Python packages. Install only with uv,
never pip. It must run on Windows, macOS, and Linux.

## Layout

- `crates/calx-core`: auth, providers, and message types. Keep provider wire
  formats inside their provider module; the rest of the code uses `message.rs`.
- `crates/calx-core/src/agent.rs`: the agent loop. Tools live in `tools/` and
  implement the `Tool` trait.
- `crates/calx`: the CLI. Keep it a thin frontend over `calx-core`.

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Build and smoke-test the wheel: `uv tool install --force . && calx --version`.

`CALX_HOME`, `CALX_AUTH_BASE_URL`, and `CALX_OPENAI_BASE_URL` point calx at a
scratch directory and test servers, so login and streaming can be exercised
without a real account.
