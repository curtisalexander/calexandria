# Backlog

Ideas and features for calx, roughly grouped. Not a commitment to build
everything here, or in this order; the README roadmap is the near-term plan.

## Context and history

- **Compaction in place**, after exe.dev's Shelley
  ([post](https://blog.exe.dev/compaction)): built (`compaction.rs`). Next:
  - backtest the linter's limits (keep 15–70%, the latest 4 items, under half
    per rewrite) on real saved sessions, and tune them;
  - look up each model's real context window instead of assuming 400k;
- **History lives in SQLite** (`sessions.db`): an append-only `items` table
  with stable per-session sequence numbers, which compaction records can point at.
  Next: full-text search over sessions (SQLite FTS5) and memory in the same database.
- **Introspection.** Let calx query its own past sessions, both for the model
  (look up compacted items) and for tuning (backtest compaction on old sessions).

## Where tools run

- **More execution environments.** Tools now act only through the `Environment`
  trait (read, write, list, exec), and `LocalEnvironment` is the only
  implementation: this machine, as the user. Add a container, VM, or SSH
  implementation so tools can run away from the harness, its credentials, and
  its history.
- **Keep credentials out of the tools' reach.** Credentials are now encrypted
  with a key in the OS keychain. On macOS the Keychain asks before another
  program reads that key, but on Windows and Linux any program running as the
  user can, including a command the `shell` tool runs. Full protection needs
  tools that run in a separate environment.
- **Codemode** ([Armin Ronacher's post](https://lucumr.pocoo.org/2026/10/6/codemode/),
  [Cloudflare's original](https://blog.cloudflare.com/code-mode/)). A sandboxed
  script runtime inside the harness where the model writes a short program that
  calls tools, MCP tools, and harness-only APIs (other models, subagents, images,
  stored values) without each intermediate result passing through its context.
  The script has no file system, network, or clock of its own: its only effects
  are tool calls, which go through the same approval rules as direct calls.

  The language is open. Candidates, all embeddable in Rust:
  - **Starlark** (the `starlark` crate, from Buck2). Python-like, so models and
    Curtis both read it easily. Hermetic and deterministic by design, so a run
    can be recorded (tool results) and replayed exactly, which suits sessions,
    compaction, and evals. Gaps: no async, so the harness must offer a builtin
    such as `parallel(fn, items)`; no `while`, exceptions, or classes, so models
    that write full Python will hit errors and need clear messages back.
  - **JavaScript via QuickJS** (`rquickjs`), what Pi uses. Models write it most
    fluently, and `Promise.all` makes concurrency natural. Not deterministic
    without extra work.
  - **A Python subset interpreter** written for agents. Worth checking what
    exists when we get here.
  - Lua (`mlua`) or Rhai: easy to embed, but models know them less well.

  Decide when there are MCP servers or harness-only APIs to orchestrate; until
  then, `shell` already lets the model compose programs. Whatever the language,
  cap concurrent tool calls (Pi uses four), memory, and run time.
- **Parallel tool calls.** The model can already request several at once; calx
  runs them one after another.

## Input

- **Voice prompts.** Speak a prompt, or dictate into the Notes aside. Transcribe
  locally where possible (for example Whisper through MLX on macOS) so audio
  never leaves the machine, with a hosted transcription API as the fallback.
  Show the transcript for editing before it is sent.
- **A live prompt in the plain view.** Marginalia reads whole lines from the
  terminal, so it can join pastes and `\` continuations but cannot edit across
  lines. Drawing its prompt with ratatui's inline viewport, with output inserted
  above it, would give it the panel view's editor, at the cost of raw mode.
- **`$EDITOR` for the notes or prompt**: open the text in real vim and read it
  back when vim exits.
