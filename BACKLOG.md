# Backlog

Ideas and features for calx, roughly grouped. Not a commitment to build
everything here, or in this order; the README roadmap is the near-term plan.

## Context and history

- **Compaction in place**, after exe.dev's Shelley
  ([post](https://blog.exe.dev/compaction)). At about 200k tokens (configurable),
  the harness adds a nudge message and gives the model a compaction tool. The tool:
  - shows the model an index of the conversation, one sequential ID per item;
  - lets it trim specific tool outputs and "squish" runs of items into shorter
    rewritten segments that point back to the originals;
  - runs a linter that rejects a compaction that removes too much or too little,
    so the model usually gets it right on the second try.

  Compaction is stored as its own record in the append-only history. When the
  harness builds a request, it swaps the compacted ranges for their rewrites, so
  the model never sees the compaction itself, and nothing is lost: the originals
  stay in the history and the model can look them up. Shelley's two earlier
  designs (restart with a summary; restart keeping recent messages) were
  replaced by this one.

  Needs first: saved sessions, stable item IDs, and a running context-size count
  (calx already reads token usage from every response).
- **Where history lives.** The README plans append-only JSONL. exe.dev chose
  SQLite for Shelley and is glad it did; SQLite would also serve memory and
  search. Decide before building sessions.
- **Introspection.** Let calx query its own past sessions, both for the model
  (look up compacted items) and for tuning (backtest compaction on old sessions).

## Where tools run

- **An execution environment boundary.** Today every tool runs on the user's
  machine as the user: file tools inside the calx process, `shell` as a child
  process. Put file and process access behind one interface (read, write, list,
  exec) with a local implementation, so tools can later run in a container, a VM,
  or over SSH while the harness, its credentials, and its history stay on the
  trusted side.
- **Keep credentials out of the tools' reach.** The `shell` tool can read
  `~/.calx/auth.json` today. Options: OS keychain storage, a scrubbed environment
  for child processes, or tools that run in a separate environment.
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
