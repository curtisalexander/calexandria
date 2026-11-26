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
  PowerShell). calx asks before each `write`, `edit`, and `shell`, showing a
  diff for edits; `-y` skips that.
- **Two views.** *Marginalia*, the default, is plain scrolling output: each item
  is numbered in the left margin (`§16`), tool times are in the right margin,
  and a status line after each turn shows context use, cache hits, and which
  legs of the lethal trifecta the session has opened. *Carrels* (`calx -p`, or
  `/panels` in a conversation) is a full-screen view with tiles for the
  conversation, the pending diff, the tool log, stats, context use, calx itself,
  and its settings. It needs 140 columns; narrower terminals get the *Reading
  room*, the conversation plus one side panel.

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
calx sessions                    # list recent sessions
calx -p                          # the panel view; -c -p resumes into it
calx -c                          # continue the latest session in this directory
calx -r 4f02f1ac "and then?"     # resume a session by the start of its ID
calx logout
```

You can type while calx works. By default a message *steers*: calx reads it
at the next pause in the turn (after the current request or tool), skips the
tool calls still waiting in that batch, and goes on with your message in mind.
`--while-busy queue` (or `CALX_WHILE_BUSY=queue`) sends it as a new prompt after
the turn instead, and `--while-busy stop` stops the turn and sends it. In the
plain view, `/steer`, `/queue`, or `/stop` before a message picks for that one
line, and answering an approval with a message instead of `y` or `n` declines
the call and steers. In the panel view, Alt-Enter does the other of steer and
queue, and Ctrl-U with an empty prompt takes back the last message not yet read.

In the panel view, Tab switches the side panel, Ctrl-L switches layouts
(coding, research, inspect), PgUp and PgDn scroll, Esc stops a running turn,
and Ctrl-D quits. The prompt grows as you type: Ctrl-J starts a new
line, and pasted text keeps its lines. Ctrl-N opens a Notes aside, a scratchpad
saved to `~/.calx/notes.md` and never sent to the model: jot things down while
calx works, or draft your next prompt as the last paragraph and press Ctrl-S to
move it into the prompt. The notes edit with vim keys (motions, `dd`, `o`, `u`,
visual mode, `/` search); they open in insert mode, and Esc in normal mode
leaves them without stopping the turn.

With `--prompt-keys vim` (or `CALX_PROMPT_KEYS=vim`) the
prompt edits with vim keys too: each message starts in insert mode, an `N` in
the margin marks normal mode, Enter sends from either mode, and during a turn
Esc stops it only from normal mode with an empty prompt.

In the plain view, a paste of several lines is one prompt, and a line that ends
with `\` continues on the next, as in a shell. When calx asks to run a tool, answer `y` yes, `n` no, `a`
always for that tool this session, or `?` for why it asks.

Every conversation is saved to `~/.calx/sessions.db` (SQLite) after each turn,
including what the tools read, so treat it as private as your files.

`calx login` opens your browser and waits for it on `127.0.0.1:1455`. Over SSH,
or when the browser cannot reach that port, paste the URL the browser ends on
into the terminal instead.

| Setting | Flag | Environment variable | Default |
|---|---|---|---|
| Model | `-m`, `--model` | `CALX_MODEL` | `gpt-6.2-sol` |
| Reasoning effort | `--effort` | `CALX_EFFORT` | `medium` |
| Typing while calx works | `--while-busy` | `CALX_WHILE_BUSY` | `steer` (or `queue`, `stop`) |
| Prompt keys in the panel view | `--prompt-keys` | `CALX_PROMPT_KEYS` | `plain` (or `vim`) |
| Shell for the `shell` tool | | `CALX_SHELL` | bash, else PowerShell |
| Encryption key, instead of the keychain | | `CALX_AUTH_KEY` | |
| State directory | | `CALX_HOME` | `~/.calx` |
| Responses API base URL | | `CALX_OPENAI_BASE_URL` | `https://api.openai.com/v1` |

Your sign-in is saved encrypted in `~/.calx/auth.json`. The key is in your OS
keychain: macOS Keychain, Windows Credential Manager, or the Linux Secret Service
(GNOME Keyring or KWallet). On a machine without a keychain, such as a headless
Linux server, set `CALX_AUTH_KEY` to a base64 string of 32 random bytes
(`openssl rand -base64 32`) and keep it as safe as a password. On macOS, expect
the Keychain to ask once after each update whether calx may use its key.

## Layout

- `crates/calx-core`: the harness itself: auth, providers, message types.
- `crates/calx`: the `calx` command-line program, a thin frontend over the core,
  with its two views (`marginalia.rs`, `carrels.rs`).
- `python/calexandria`: the Python package that ships the binary (built with maturin).

## Roadmap

1. More tools (search, find files) and saved approval rules.
2. More model backends: Chat Completions (local MLX, Ollama, work gateways),
   API keys, and a configurable OAuth flow for work.
3. Terminal UI next steps: a Trace view of every request in a turn, and
   editable Prefs.
4. Memory, and extensions written in Python.

Longer-term ideas, such as compaction and where tools run, are in
[BACKLOG.md](BACKLOG.md).
