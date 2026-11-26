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
- **Codemode** ([Armin Ronacher's post](https://lucumr.pocoo.org/2026/10/6/codemode/)).
  A sandboxed script runtime inside the harness (QuickJS, or Starlark for
  deterministic replay) where the model composes many tool calls, MCP calls,
  and harness-only calls (other models, subagents, images) without each result
  passing through its context. Worth it once there are MCP servers or harness
  APIs to orchestrate.
- **Parallel tool calls.** The model can already request several at once; calx
  runs them one after another.
