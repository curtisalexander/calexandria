mod carrels;
mod editor;
mod marginalia;
mod notes;
mod status;

use std::io::BufRead;

use anyhow::{Context, Result};
use calx_core::agent::Agent;
use calx_core::auth::{ChatGptAuth, CredentialStore, PendingLogin};
use calx_core::environment::{Environment, LocalEnvironment};
use calx_core::responses::ResponsesClient;
use calx_core::session::Sessions;
use calx_core::tools::Toolbox;
use clap::{Parser, Subcommand};
use marginalia::one_line;

const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

/// Calexandria: a personal agent harness.
#[derive(Parser)]
#[command(name = "calx", version, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Ask one question and exit. Without it, calx starts a conversation.
    prompt: Vec<String>,

    /// The model to use.
    #[arg(long, short, env = "CALX_MODEL", default_value = "gpt-6.2-sol")]
    model: String,

    /// Reasoning effort: none, low, medium, high, or xhigh.
    #[arg(long, env = "CALX_EFFORT", default_value = "medium")]
    effort: String,

    /// Run file edits and shell commands without asking first.
    #[arg(long, short = 'y')]
    yes: bool,

    /// Continue the most recent session in this directory.
    #[arg(long = "continue", short = 'c', conflicts_with = "resume")]
    continue_latest: bool,

    /// Resume a session by its ID, or the start of it; see `calx sessions`.
    #[arg(long, short = 'r', value_name = "ID")]
    resume: Option<String>,

    /// What a prompt sent while calx is working does: steer the running turn
    /// at its next pause, queue it for after the turn, or stop the turn and send it.
    #[arg(long, env = "CALX_WHILE_BUSY", value_enum, default_value_t = status::Busy::Steer)]
    while_busy: status::Busy,

    /// Keys for editing the prompt in the panel view: plain, or vim.
    #[arg(long, env = "CALX_PROMPT_KEYS", value_enum, default_value_t = editor::Keys::Plain)]
    prompt_keys: editor::Keys,

    /// Start in the panel view instead of plain scrolling output.
    #[arg(long, short = 'p', conflicts_with = "prompt")]
    panels: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in with ChatGPT, so calx uses your ChatGPT plan.
    Login,
    /// Remove the saved ChatGPT sign-in.
    Logout,
    /// List recent sessions.
    Sessions,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Login) => login().await,
        Some(Command::Logout) => logout(),
        Some(Command::Sessions) => list_sessions(),
        None => chat(&cli).await,
    }
}

async fn login() -> Result<()> {
    let pending = PendingLogin::start().await?;
    println!(
        "Sign in to ChatGPT in your browser. If it does not open, visit:\n\n  {}\n",
        pending.url
    );
    let _ = open::that(&pending.url);
    println!(
        "If the browser cannot reach this machine (for example over SSH), paste the URL it ends on here:"
    );

    // Read stdin on a plain thread: a pending blocking read would otherwise keep
    // the async runtime from shutting down after a browser callback.
    let (tx, mut pasted) = tokio::sync::mpsc::channel(1);
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if !line.trim().is_empty() && tx.blocking_send(line).is_err() {
                break;
            }
        }
    });

    let code = loop {
        tokio::select! {
            result = pending.wait_for_callback() => break result?,
            Some(line) = pasted.recv() => match pending.parse_pasted(&line) {
                Ok(code) => break code,
                Err(e) => eprintln!("{e}"),
            },
        }
    };
    let credential = pending.finish(code).await?;
    let store = CredentialStore::open()?;
    store.save(Some(&credential))?;
    println!("Signed in. Saved to {}", store.path().display());
    Ok(())
}

fn logout() -> Result<()> {
    let store = CredentialStore::open()?;
    store.save(None)?;
    println!("Signed out.");
    Ok(())
}

fn system_prompt(env: &dyn Environment) -> String {
    format!(
        "You are calx (Calexandria), Curtis Alexander's personal assistant, running in a terminal.\n\
         Be direct and concise. Format for a terminal: short paragraphs, plain lists, fenced code.\n\
         You can read, search, write, and edit files and run {} commands with your tools. Use \
         them to check facts about this machine instead of guessing. Search with grep and \
         find rather than the shell; they need no approval. Read a file before editing it.\n\
         Operating system: {}. Working directory: {}.",
        env.shell_name(),
        env.os(),
        env.working_dir()
    )
}

fn list_sessions() -> Result<()> {
    let sessions = Sessions::open()?.list(20)?;
    if sessions.is_empty() {
        println!("No sessions yet.");
        return Ok(());
    }
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    for s in sessions {
        let place = if s.cwd == cwd {
            String::new()
        } else {
            format!("  {DIM}{}{RESET}", s.cwd)
        };
        println!(
            "{}  {DIM}{:>8}  {:>4} items{RESET}  {}{place}",
            &s.id[..8.min(s.id.len())],
            ago(s.updated_at),
            s.item_count,
            one_line(&s.title, 60)
        );
    }
    println!("\n{DIM}Resume one with `calx -r <id>`, or the latest here with `calx -c`.{RESET}");
    Ok(())
}

fn ago(ms: u64) -> String {
    let secs = calx_core::session::now_ms().saturating_sub(ms) / 1000;
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

async fn chat(cli: &Cli) -> Result<()> {
    let auth = ChatGptAuth::load(CredentialStore::open()?)?;
    let env = LocalEnvironment::current()?;
    let cwd = env.working_dir();
    let shell = env.shell_name().to_string();
    let sessions = Sessions::open()?;
    let resumed =
        match (&cli.resume, cli.continue_latest) {
            (Some(prefix), _) => Some(sessions.resolve(prefix)?),
            (None, true) => Some(sessions.latest(&cwd)?.context(
                "no earlier session in this directory; `calx sessions` lists all of them",
            )?),
            (None, false) => None,
        };
    let (session_id, items) = match resumed {
        Some(id) => {
            let items = sessions.load(&id)?;
            (id, items)
        }
        None => (calx_core::new_id(), Vec::new()),
    };
    let mut agent = Agent {
        client: ResponsesClient::new(auth),
        system: system_prompt(env.as_ref()),
        toolbox: Toolbox::builtin(env),
        model: cli.model.clone(),
        effort: (cli.effort != "none").then(|| cli.effort.clone()),
        session_id,
        items,
    };
    // Saves the items a finished turn added. A turn that fails or is
    // interrupted is not saved, matching what the next turn sends the model.
    let save = |agent: &Agent, before: usize| {
        let new = &agent.items[before..];
        if let Err(e) = sessions.append(&agent.session_id, &cwd, &agent.model, new) {
            eprintln!("{RED}warning:{RESET} could not save the session: {e:#}");
        }
    };
    if !agent.items.is_empty() && !cli.panels {
        println!(
            "{DIM}Resumed session {} ({} items).{RESET}",
            &agent.session_id[..8],
            agent.items.len()
        );
    }

    let mut simple =
        marginalia::Marginalia::new(cli.yes, status::Tally::new(&agent), cli.while_busy);
    if !cli.prompt.is_empty() {
        let before = agent.items.len();
        if simple.turn(&mut agent, &cli.prompt.join(" ")).await? {
            save(&agent, before);
        }
        return Ok(());
    }
    let panels = cli.panels
        || matches!(
            simple.converse(&mut agent, &save).await?,
            marginalia::Exit::Panels
        );
    if panels {
        let info = carrels::Info::new(&agent, &shell, &cwd);
        let mut app = carrels::App::new(&agent, info, cli.yes, std::mem::take(&mut simple.tally));
        app.always = simple.always;
        app.busy = cli.while_busy;
        app.set_prompt_keys(cli.prompt_keys);
        carrels::run(app, &mut agent, &save).await?;
    }
    Ok(())
}
