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
- `crates/calx-core/src/environment.rs`: where tools act. Tools must do all file
  and process work through the `Environment` trait, never `std::fs` or
  `std::process` directly, so they can later run in a container, VM, or over SSH
  while the harness and its credentials stay put.
- `crates/calx`: the CLI and its views. `marginalia.rs` is the plain scrolling
  view, `carrels.rs` the full-screen ratatui view, and `status.rs` what both
  report. Views learn what the agent does only from `Event`s and `Approval`s;
  when a view needs more, add an event in the core rather than reaching into it.
- `crates/calx-core/src/store.rs`: secrets, encrypted with a key in the OS keychain.
- `crates/calx-core/src/session.rs`: sessions in SQLite (`sessions.db`), an
  append-only list of items per session numbered from 1. Never rewrite saved
  items; later features such as compaction add records that refer to them.

## Security

Design with the "lethal trifecta" in mind: untrusted content, private data, and
a way to send data out. Assume anything a tool reads may contain instructions.
Keep credentials out of what tools can reach: environment variables the harness
uses are hidden from commands (`HIDDEN_VARS` in `environment.rs`).
- `crates/calx`: the CLI. Keep it a thin frontend over `calx-core`.

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Build and smoke-test the wheel: `uv tool install --force . && calx --version`.

`CALX_HOME`, `CALX_AUTH_KEY`, `CALX_AUTH_BASE_URL`, and `CALX_OPENAI_BASE_URL`
point calx at a scratch directory, a key that bypasses the OS keychain, and test
servers, so login and streaming can be exercised
without a real account.
