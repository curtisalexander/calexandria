# Calexandria

A personal agent harness, named for the Library of Alexandria: an agent that
remembers. The command is **`calx`**.

calx is written in Rust and packaged as a Python wheel, so `uv tool install`
puts the `calx` binary on your PATH on Windows, macOS, and Linux.

## Status

Early. Today calx can:

- **Sign in with ChatGPT** (`calx login`), so requests use your ChatGPT plan's
  allowance instead of API credits. Tokens refresh on their own.
- **Work as an agent** over OpenAI's Responses API: it streams the reply and a
  summary of the model's reasoning, and calls tools until the task is done.
- **Tools:** `read` (files and directories), `write`, `edit` (exact text
  replacement), and `shell` (bash, which is Git Bash on Windows, otherwise
  PowerShell). calx asks before each `write`, `edit`, and `shell`; `-y` skips that.

## Install

```sh
uv tool install git+https://github.com/curtisalexander/calexandria
```

From a checkout, `uv tool install .`; after pulling changes,
`uv tool install --force .`. Building needs a Rust toolchain.

## Use

```sh
calx login                       # sign in with ChatGPT in your browser
calx "what is a fat-tailed distribution?"   # ask once
calx                             # start a conversation; /exit to quit
calx -m gpt-6.2-astra --effort high
calx -y "run the tests and fix what fails"   # no approval prompts
calx logout
```

`calx login` opens your browser and waits for it on `127.0.0.1:1455`. Over SSH,
or when the browser cannot reach that port, paste the URL the browser ends on
into the terminal instead.

| Setting | Flag | Environment variable | Default |
|---|---|---|---|
| Model | `-m`, `--model` | `CALX_MODEL` | `gpt-6.2-sol` |
| Reasoning effort | `--effort` | `CALX_EFFORT` | `medium` |
| Shell for the `shell` tool | | `CALX_SHELL` | bash, else PowerShell |
| State directory | | `CALX_HOME` | `~/.calx` |
| Responses API base URL | | `CALX_OPENAI_BASE_URL` | `https://api.openai.com/v1` |

Your sign-in is saved in `~/.calx/auth.json`, readable only by you.

## Layout

- `crates/calx-core`: the harness itself: auth, providers, message types.
- `crates/calx`: the `calx` command-line program, a thin frontend over the core.
- `python/calexandria`: the Python package that ships the binary (built with maturin).

## Roadmap

1. More tools (search, find files), cancelling a running turn, and approval rules.
2. Sessions saved as append-only JSONL: resume, replay, and evals.
3. More model backends: Chat Completions (local MLX, Ollama, work gateways),
   API keys, and a configurable OAuth flow for work.
4. A terminal UI.
5. Memory, and extensions written in Python.

Longer-term ideas, such as compaction and where tools run, are in
[BACKLOG.md](BACKLOG.md).
