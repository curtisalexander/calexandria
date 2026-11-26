use std::io::{BufRead, Write};

use anyhow::Result;
use calx_core::auth::{ChatGptAuth, CredentialStore, PendingLogin};
use calx_core::message::{Event, Message};
use calx_core::responses::{Request, ResponsesClient};
use clap::{Parser, Subcommand};

const DIM: &str = "\x1b[2m";
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
}

#[derive(Subcommand)]
enum Command {
    /// Sign in with ChatGPT, so calx uses your ChatGPT plan.
    Login,
    /// Remove the saved ChatGPT sign-in.
    Logout,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Login) => login().await,
        Some(Command::Logout) => logout(),
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

fn system_prompt() -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    format!(
        "You are calx (Calexandria), Curtis Alexander's personal assistant, running in a terminal.\n\
         Be direct and concise. Format for a terminal: short paragraphs, plain lists, fenced code.\n\
         Operating system: {}. Working directory: {cwd}.",
        std::env::consts::OS
    )
}

async fn chat(cli: &Cli) -> Result<()> {
    let auth = ChatGptAuth::load(CredentialStore::open()?)?;
    let client = ResponsesClient::new(auth);
    let system = system_prompt();
    let session_id = calx_core::new_id();
    let effort = (cli.effort != "none").then_some(cli.effort.as_str());
    let mut messages = Vec::new();

    if !cli.prompt.is_empty() {
        messages.push(Message::user(cli.prompt.join(" ")));
        let request = Request {
            model: &cli.model,
            system: &system,
            messages: &messages,
            effort,
            session_id: &session_id,
        };
        return turn(&client, &request).await.map(|_| ());
    }

    println!("{DIM}calx · {} · /exit to quit{RESET}", cli.model);
    let stdin = std::io::stdin();
    loop {
        print!("\n> ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        match line {
            "" => continue,
            "/exit" | "/quit" => break,
            _ => {}
        }
        messages.push(Message::user(line));
        let request = Request {
            model: &cli.model,
            system: &system,
            messages: &messages,
            effort,
            session_id: &session_id,
        };
        match turn(&client, &request).await {
            Ok(text) => messages.push(Message::assistant(text)),
            Err(e) => {
                // Drop the unanswered question so the next turn starts clean.
                messages.pop();
                eprintln!("\nerror: {e:#}");
            }
        }
    }
    Ok(())
}

/// Streams one reply to the terminal and returns its text.
async fn turn(client: &ResponsesClient, request: &Request<'_>) -> Result<String> {
    let mut out = std::io::stdout();
    let mut in_reasoning = false;
    let reply = client
        .stream(request, |event| {
            match event {
                Event::ReasoningDelta(delta) => {
                    if !in_reasoning {
                        in_reasoning = true;
                        let _ = write!(out, "{DIM}");
                    }
                    let _ = write!(out, "{delta}");
                }
                Event::TextDelta(delta) => {
                    if in_reasoning {
                        in_reasoning = false;
                        let _ = write!(out, "{RESET}\n\n");
                    }
                    let _ = write!(out, "{delta}");
                }
            }
            let _ = out.flush();
        })
        .await;
    if in_reasoning {
        print!("{RESET}");
    }
    let reply = reply?;
    let u = reply.usage;
    println!(
        "\n{DIM}[{} in, {} cached · {} out]{RESET}",
        u.input_tokens, u.cached_input_tokens, u.output_tokens
    );
    Ok(reply.text)
}
