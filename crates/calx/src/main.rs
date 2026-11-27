mod carrels;
mod editor;
mod marginalia;
mod notes;
mod status;

use std::io::BufRead;

use anyhow::{Context, Result};
use calx_core::agent::Agent;
use calx_core::approvals::Rules;
use calx_core::auth::{API_KEY_ENV, Auth, ChatGptAuth, CredentialStore, PendingLogin};
use calx_core::environment::{Environment, LocalEnvironment};
use calx_core::gateway::{DEFAULT_PORT, GatewayAuth, GatewayConfig, PendingGatewayLogin};
use calx_core::provider::{Api, Client, OPENAI_BASE_URL};
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

    /// The model to use. Default with the Responses API: gpt-6.2-sol.
    #[arg(long, short, env = "CALX_MODEL")]
    model: Option<String>,

    /// Reasoning effort: none, low, medium, high, or xhigh. Default: medium
    /// with the Responses API; with Chat Completions, none is sent.
    #[arg(long, env = "CALX_EFFORT")]
    effort: Option<String>,

    /// The API the model server speaks: OpenAI's Responses API, or Chat
    /// Completions, which Ollama, MLX, vLLM, OpenRouter, and most gateways speak.
    #[arg(long, env = "CALX_API", value_enum, default_value_t = ApiArg::Responses)]
    api: ApiArg,

    /// The server's base URL, such as http://localhost:11434/v1 for Ollama.
    /// Default with the Responses API: https://api.openai.com/v1.
    #[arg(long, env = "CALX_BASE_URL")]
    base_url: Option<String>,

    /// Use the API key saved under this name (`calx key set NAME`) instead of
    /// the ChatGPT sign-in. Without it, Chat Completions sends no key.
    #[arg(
        long,
        env = "CALX_KEY",
        value_name = "NAME",
        conflicts_with = "gateway"
    )]
    key: Option<String>,

    /// Authenticate with the gateway sign-in saved under this name
    /// (`calx gateway login NAME`).
    #[arg(long, env = "CALX_GATEWAY", value_name = "NAME")]
    gateway: Option<String>,

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

#[derive(Clone, Copy, clap::ValueEnum)]
enum ApiArg {
    Responses,
    Chat,
}

#[derive(Subcommand)]
enum KeyCommand {
    /// Save an API key under NAME; it is read from the terminal, hidden, or from stdin.
    Set { name: String },
    /// List the names of saved keys.
    List,
    /// Remove a saved key.
    Remove { name: String },
}

#[derive(Subcommand)]
enum GatewayCommand {
    /// Sign in to a gateway in the browser. Later logins reuse its URLs.
    Login {
        name: String,
        /// Where the browser starts the sign-in.
        #[arg(long)]
        start_url: Option<String>,
        /// Where calx exchanges the sign-in for access tokens.
        #[arg(long)]
        token_url: Option<String>,
        /// The OAuth client ID, if the gateway wants one.
        #[arg(long)]
        client_id: Option<String>,
        /// The localhost port the browser comes back to (default 1456).
        #[arg(long)]
        port: Option<u16>,
    },
    /// List the gateways you are signed in to.
    List,
    /// Remove a gateway sign-in.
    Logout { name: String },
}

#[derive(Subcommand)]
enum Command {
    /// Sign in with ChatGPT, so calx uses your ChatGPT plan.
    Login,
    /// Remove the saved ChatGPT sign-in.
    Logout,
    /// List recent sessions.
    Sessions,
    /// Save, list, or remove API keys, kept encrypted with a key in the OS keychain.
    #[command(subcommand)]
    Key(KeyCommand),
    /// Sign in to a company model gateway; its token is refreshed in the background.
    #[command(subcommand)]
    Gateway(GatewayCommand),
    /// List the approvals saved with `p` at an approval prompt.
    Rules {
        /// Remove rule N, as numbered in the list.
        #[arg(long, value_name = "N")]
        forget: Option<usize>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Login) => login().await,
        Some(Command::Logout) => logout(),
        Some(Command::Sessions) => list_sessions(),
        Some(Command::Key(command)) => key(command),
        Some(Command::Gateway(command)) => gateway(command).await,
        Some(Command::Rules { forget }) => rules(forget),
        None => chat(&cli).await,
    }
}

async fn login() -> Result<()> {
    let pending = PendingLogin::start().await?;
    let code = browser_sign_in(
        "Sign in to ChatGPT",
        &pending.url,
        pending.wait_for_callback(),
        |line| pending.parse_pasted(line),
    )
    .await?;
    let credential = pending.finish(code).await?;
    let store = CredentialStore::open()?;
    store.save(Some(&credential))?;
    println!("Signed in. Saved to {}", store.path().display());
    Ok(())
}

/// Opens `url` in the browser and waits for the sign-in to come back, either
/// to the local callback server or as a URL the user pastes.
async fn browser_sign_in<T>(
    what: &str,
    url: &str,
    callback: impl std::future::Future<Output = Result<T>>,
    parse_pasted: impl Fn(&str) -> Result<T>,
) -> Result<T> {
    println!("{what} in your browser. If it does not open, visit:\n\n  {url}\n");
    let _ = open::that(url);
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
    tokio::pin!(callback);
    loop {
        tokio::select! {
            result = &mut callback => return result,
            Some(line) = pasted.recv() => match parse_pasted(&line) {
                Ok(grant) => return Ok(grant),
                Err(e) => eprintln!("{e}"),
            },
        }
    }
}

async fn gateway(command: GatewayCommand) -> Result<()> {
    let store = CredentialStore::open()?;
    match command {
        GatewayCommand::Login {
            name,
            start_url,
            token_url,
            client_id,
            port,
        } => {
            let saved = store.gateway(&name)?.map(|c| c.config);
            let config = match (start_url, token_url, saved) {
                (Some(start_url), Some(token_url), _) => GatewayConfig {
                    start_url,
                    token_url,
                    client_id,
                    port: port.unwrap_or(DEFAULT_PORT),
                },
                (None, None, Some(mut saved)) => {
                    saved.client_id = client_id.or(saved.client_id);
                    saved.port = port.unwrap_or(saved.port);
                    saved
                }
                (None, None, None) => {
                    anyhow::bail!("the first login to {name} needs --start-url and --token-url")
                }
                _ => anyhow::bail!("give both --start-url and --token-url, or neither"),
            };
            let pending = PendingGatewayLogin::start(config).await?;
            let grant = browser_sign_in(
                &format!("Sign in to {name}"),
                &pending.url,
                pending.wait_for_callback(),
                |line| pending.parse_pasted(line),
            )
            .await?;
            let credential = pending.finish(grant).await?;
            store.set_gateway(&name, Some(&credential))?;
            println!(
                "Signed in to {name}. Use it with `calx --gateway {name} --api chat --base-url <URL> -m <model>`."
            );
        }
        GatewayCommand::List => {
            let names = store.gateway_names()?;
            if names.is_empty() {
                println!(
                    "No gateways. Sign in to one with `calx gateway login NAME --start-url URL --token-url URL`."
                );
            }
            for name in names {
                let c = store.gateway(&name)?.context("listed but missing")?;
                println!("{name}  {DIM}{}{RESET}", c.config.start_url);
            }
        }
        GatewayCommand::Logout { name } => {
            anyhow::ensure!(store.gateway(&name)?.is_some(), "not signed in to {name}");
            store.set_gateway(&name, None)?;
            println!("Signed out of {name}.");
        }
    }
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

fn key(command: KeyCommand) -> Result<()> {
    let store = CredentialStore::open()?;
    match command {
        KeyCommand::Set { name } => {
            let key = read_secret(&format!("API key for {name}: "))?;
            anyhow::ensure!(!key.is_empty(), "no key given");
            store.set_api_key(&name, Some(&key))?;
            println!("Saved key {name}. Use it with `calx --key {name}`.");
        }
        KeyCommand::List => {
            let names = store.api_key_names()?;
            if names.is_empty() {
                println!("No saved keys. Add one with `calx key set NAME`.");
            }
            for name in names {
                println!("{name}");
            }
        }
        KeyCommand::Remove { name } => {
            anyhow::ensure!(
                store.api_key(&name)?.is_some(),
                "there is no key named {name}"
            );
            store.set_api_key(&name, None)?;
            println!("Removed key {name}.");
        }
    }
    Ok(())
}

/// Reads a line without showing it when stdin is a terminal.
fn read_secret(prompt: &str) -> Result<String> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        return Ok(line.trim().to_string());
    }
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    crossterm::terminal::enable_raw_mode()?;
    let mut text = String::new();
    let result = loop {
        match read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => match k.code {
                KeyCode::Enter => break Ok(()),
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err(anyhow::anyhow!("cancelled"));
                }
                KeyCode::Char(c) => text.push(c),
                KeyCode::Backspace => {
                    text.pop();
                }
                _ => {}
            },
            Ok(Event::Paste(p)) => text.push_str(&p),
            Ok(_) => {}
            Err(e) => break Err(e.into()),
        }
    };
    crossterm::terminal::disable_raw_mode()?;
    eprintln!();
    result.map(|()| text.trim().to_string())
}

/// The model client the flags choose.
fn client(cli: &Cli) -> Result<(Client, String, Option<String>)> {
    let api = match cli.api {
        ApiArg::Responses => Api::Responses,
        ApiArg::Chat => Api::Chat,
    };
    let base_url = match (&cli.base_url, api) {
        (Some(url), _) => url.clone(),
        (None, Api::Responses) => OPENAI_BASE_URL.to_string(),
        (None, Api::Chat) => anyhow::bail!(
            "Chat Completions needs --base-url (or CALX_BASE_URL), such as \
             http://localhost:11434/v1 for Ollama or http://localhost:8080/v1 for mlx_lm.server"
        ),
    };
    let auth = if let Ok(key) = std::env::var(API_KEY_ENV) {
        Auth::Key {
            name: API_KEY_ENV.into(),
            key,
        }
    } else if let Some(name) = &cli.gateway {
        Auth::Gateway(GatewayAuth::load(name, CredentialStore::open()?)?)
    } else if let Some(name) = &cli.key {
        let key = CredentialStore::open()?
            .api_key(name)?
            .with_context(|| format!("no key named {name}; save one with `calx key set {name}`"))?;
        Auth::Key {
            name: name.clone(),
            key,
        }
    } else if api == Api::Responses {
        Auth::ChatGpt(ChatGptAuth::load(CredentialStore::open()?)?)
    } else {
        Auth::None
    };
    let model = match (&cli.model, api) {
        (Some(model), _) => model.clone(),
        (None, Api::Responses) => "gpt-6.2-sol".to_string(),
        (None, Api::Chat) => anyhow::bail!("name the model with -m, such as -m qwen3:8b"),
    };
    let effort = match (&cli.effort, api) {
        (Some(e), _) if e == "none" => None,
        (Some(e), _) => Some(e.clone()),
        (None, Api::Responses) => Some("medium".to_string()),
        (None, Api::Chat) => None,
    };
    Ok((Client::new(api, &base_url, auth), model, effort))
}

fn rules(forget: Option<usize>) -> Result<()> {
    let mut rules = Rules::load(&std::env::current_dir()?)?;
    if let Some(n) = forget {
        let rule = rules.remove(n.checked_sub(1).context("rules are numbered from 1")?)?;
        println!("Removed: always {} in {}", rule.label(), rule.project);
        return Ok(());
    }
    if rules.rules.is_empty() {
        println!("No saved rules. Answer `p` when calx asks to run a tool to save one.");
        return Ok(());
    }
    let here = rules.project().display().to_string();
    for (i, rule) in rules.rules.iter().enumerate() {
        let place = if rule.project == here {
            format!("{DIM}this project{RESET}")
        } else {
            format!("{DIM}{}{RESET}", rule.project)
        };
        println!("{:>3}  {}  {place}", i + 1, rule.label());
    }
    println!(
        "\n{DIM}Saved in {}. Remove one with `calx rules --forget <N>`.{RESET}",
        rules.path().display()
    );
    Ok(())
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
    let (client, model, effort) = client(cli)?;
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
        client,
        system: system_prompt(env.as_ref()),
        toolbox: Toolbox::builtin(env),
        model,
        effort,
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

    let rules = Rules::load(std::path::Path::new(&cwd))?;
    let mut simple =
        marginalia::Marginalia::new(cli.yes, rules, status::Tally::new(&agent), cli.while_busy);
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
        let tally = std::mem::take(&mut simple.tally);
        let mut app = carrels::App::new(&agent, info, cli.yes, simple.rules, tally);
        app.always = simple.always;
        app.busy = cli.while_busy;
        app.set_prompt_keys(cli.prompt_keys);
        carrels::run(app, &mut agent, &save).await?;
    }
    Ok(())
}
