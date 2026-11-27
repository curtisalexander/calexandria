//! Marginalia, the simple view: plain scrolling output that works in any
//! terminal, over SSH, and in scrollback. The left margin numbers each item
//! (`§16`) and the right margin notes how long a tool took.

use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, IsTerminal, Write};

use anyhow::Result;
use calx_core::agent::{Agent, Approval, DECLINED, SKIPPED};
use calx_core::approvals::{Rule, Rules};
use calx_core::message::Event;

use tokio::sync::{Notify, mpsc};

use crate::status::{self, Busy, Tally};

const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const BLUE: &str = "\x1b[34m";
const RESET: &str = "\x1b[0m";

/// Width of the `§16  ` margin.
const MARGIN: usize = 6;
/// Where tool output and diffs start, under the tool's name.
const BODY: &str = "        ";

/// How a conversation in this view ended.
pub enum Exit {
    Quit,
    /// The user asked for the panel view with `/panels`.
    Panels,
}

pub struct Marginalia {
    /// Run every tool without asking (`-y`).
    pub yes: bool,
    /// Tools the user answered "always" for, this session.
    pub always: HashSet<String>,
    /// Approvals saved for this project.
    pub rules: Rules,
    pub tally: Tally,
    /// What a line typed while a turn runs does.
    busy: Busy,
    /// Lines from stdin, read on a thread that starts when first needed, so
    /// the panel view gets the terminal to itself when calx starts in it.
    lines: Option<mpsc::UnboundedReceiver<String>>,
    /// Lines read but not used yet: approval answers and, from a pipe, prompts.
    typed: VecDeque<String>,
    /// Prompts to send next, before reading more.
    queued: VecDeque<String>,
    /// Whether stdin is a terminal. Piped input is read strictly in order,
    /// as answers and prompts, never as steering.
    interactive: bool,
}

/// Where the output cursor is, so the next piece starts in the right place.
#[derive(Default, PartialEq)]
enum Line {
    #[default]
    Fresh,
    Reasoning,
    Text,
    /// A tool's line is printed without its newline, `width` columns wide,
    /// waiting for its time in the right margin.
    Tool {
        width: usize,
    },
    /// A tool's line was ended to ask for approval; its time goes on the
    /// first line of its output instead.
    Asked,
}

impl Marginalia {
    pub fn new(yes: bool, rules: Rules, tally: Tally, busy: Busy) -> Self {
        Self {
            yes,
            always: HashSet::new(),
            rules,
            tally,
            busy,
            lines: None,
            typed: VecDeque::new(),
            queued: VecDeque::new(),
            interactive: std::io::stdin().is_terminal(),
        }
    }

    /// The next prompt: a line, a paste of several lines, or lines joined
    /// with a `\` at the end of each but the last.
    async fn next_line(&mut self) -> Option<String> {
        if let Some(line) = self.typed.pop_front() {
            return Some(line);
        }
        let rx = self.lines.get_or_insert_with(spawn_reader);
        let line = rx.recv().await?;
        Some(if self.interactive {
            gather(rx, line, true).await
        } else {
            line
        })
    }

    /// Reads prompts and runs them until the user quits.
    pub async fn converse(
        &mut self,
        agent: &mut Agent,
        save: &dyn Fn(&Agent, usize),
    ) -> Result<Exit> {
        println!(
            "{DIM}calx · {} · session {} · type while it works to {} · /compact · /panels for panels · /exit to quit{RESET}",
            agent.model,
            &agent.session_id[..8],
            self.busy.name()
        );
        loop {
            print!("\n{DIM}§{:<4}{RESET}{BOLD}>{RESET} ", agent.items.len() + 1);
            let line = match self.queued.pop_front() {
                Some(line) => {
                    println!("{line}  {DIM}(sent while busy){RESET}");
                    line
                }
                None => {
                    std::io::stdout().flush()?;
                    let Some(line) = self.next_line().await else {
                        return Ok(Exit::Quit);
                    };
                    line
                }
            };
            match line.trim() {
                "" => continue,
                "/exit" | "/quit" => return Ok(Exit::Quit),
                "/panels" => return Ok(Exit::Panels),
                "/compact" => {
                    let before = agent.items.len();
                    let tally = &mut self.tally;
                    let mut on_event = |event: Event| {
                        tally.observe(&event);
                        print_compaction(&event);
                    };
                    if agent.compact(&mut on_event).await {
                        save(agent, before);
                    }
                }
                prompt => {
                    let before = agent.items.len();
                    match self.turn(agent, prompt).await {
                        Ok(true) => save(agent, before),
                        // Stopped: drop it, as for a failure.
                        Ok(false) => agent.items.truncate(before),
                        Err(e) => {
                            // Drop the failed turn so the next one starts from a valid conversation.
                            agent.items.truncate(before);
                            eprintln!("\n{RED}error:{RESET} {e:#}");
                        }
                    }
                }
            }
        }
    }

    /// Runs one prompt, printing the reply and tool activity as it happens.
    /// Returns false when the user stopped it; the caller then drops its items.
    ///
    /// A line typed meanwhile answers a pending approval, or else does what
    /// `busy` says: steer, queue, or stop. `/steer`, `/queue`, and `/stop`
    /// before the text choose for one line.
    pub async fn turn(&mut self, agent: &mut Agent, prompt: &str) -> Result<bool> {
        self.tally.start_turn();
        let width = terminal_width();
        let line = RefCell::new(Line::Fresh);
        let tally = RefCell::new(std::mem::take(&mut self.tally));
        let (yes, always) = (self.yes, RefCell::new(std::mem::take(&mut self.always)));
        let rules = &mut self.rules;
        let rules = RefCell::new(rules);
        let typed = RefCell::new(std::mem::take(&mut self.typed));
        let steering = RefCell::new(Vec::<String>::new());
        // Whether an approval waits for a line, and whether stdin has ended.
        let (asking, ended) = (Cell::new(false), Cell::new(false));
        let arrived = Notify::new();

        // Output errors are ignored throughout: a closed pipe (`calx ... | head`)
        // must not panic before the caller saves the turn.
        let mut on_event = |event: Event| {
            tally.borrow_mut().observe(&event);
            let mut out = std::io::stdout();
            let mut line = line.borrow_mut();
            match event {
                Event::ReasoningDelta(delta) => {
                    if *line != Line::Reasoning {
                        end_line(&mut out, &mut line);
                        let _ = write!(out, "{DIM}{:MARGIN$}", "");
                        *line = Line::Reasoning;
                    }
                    let _ = write!(out, "{}", indent(&delta));
                }
                Event::TextDelta(delta) => {
                    if *line != Line::Text {
                        end_line(&mut out, &mut line);
                        let _ = write!(out, "{:MARGIN$}", "");
                        *line = Line::Text;
                    }
                    let _ = write!(out, "{}", indent(&delta));
                }
                Event::ToolStarted {
                    seq, name, summary, ..
                } => {
                    end_line(&mut out, &mut line);
                    let summary = one_line(&summary, width.saturating_sub(MARGIN + 30));
                    let _ = write!(out, "{DIM}§{seq:<4}{RESET} {BOLD}▸ {name}{RESET} {summary}");
                    *line = Line::Tool {
                        width: MARGIN + 3 + name.chars().count() + summary.chars().count(),
                    };
                }
                Event::ToolFinished {
                    output,
                    is_error,
                    duration_ms,
                    ..
                } => {
                    let note = format!(
                        "{} · ~{} tok",
                        status::duration(duration_ms),
                        status::tokens(status::estimate(&output))
                    );
                    let mut note = Some(note);
                    if let Line::Tool { width: used } = *line {
                        let note = note.take().unwrap_or_default();
                        let _ = writeln!(out, "{}", right(used, &note, width, DIM));
                    }
                    *line = Line::Fresh;
                    if output == DECLINED {
                        let _ = writeln!(out, "{BODY}{DIM}declined{RESET}");
                    } else if output == SKIPPED {
                        let _ = writeln!(out, "{BODY}{DIM}skipped, you sent a message{RESET}");
                    } else {
                        let color = if is_error { RED } else { DIM };
                        let lines: Vec<&str> = output.lines().collect();
                        let max = width.saturating_sub(BODY.len() + 2);
                        for text in lines.iter().take(4) {
                            let shown = one_line(text, max.saturating_sub(24));
                            let after = match note.take() {
                                Some(n) => {
                                    right(BODY.len() + shown.chars().count(), &n, width, DIM)
                                }
                                None => String::new(),
                            };
                            let _ = writeln!(out, "{BODY}{color}{shown}{RESET}{after}");
                        }
                        if lines.len() > 4 {
                            let _ =
                                writeln!(out, "{BODY}{DIM}… {} more lines{RESET}", lines.len() - 4);
                        }
                    }
                }
                Event::Steered { seq, text } => {
                    end_line(&mut out, &mut line);
                    let text = one_line(&text, width.saturating_sub(MARGIN + 5));
                    let _ = writeln!(out, "\n{DIM}§{seq:<4}{RESET}{BOLD}you{RESET} {text}");
                }
                Event::RequestStarted { .. } | Event::RequestFinished { .. } => {}
                event => {
                    end_line(&mut out, &mut line);
                    print_compaction(&event);
                }
            }
            let _ = out.flush();
        };
        let mut approve = async |ask: Approval| -> bool {
            if yes || always.borrow().contains(&ask.name) || rules.borrow().allowing(&ask).is_some()
            {
                return true;
            }
            let rule = Rule::suggest(rules.borrow().project(), &ask);
            let for_project = rule
                .as_ref()
                .map(|r| format!("{BOLD}p{RESET} always {} in this project  ", r.label()))
                .unwrap_or_default();
            let mut out = std::io::stdout();
            if let Line::Tool { width: used } = *line.borrow() {
                let _ = writeln!(out, "{}", right(used, "waiting for you", width, YELLOW));
            }
            *line.borrow_mut() = Line::Asked;
            if let Some(preview) = &ask.preview {
                for text in preview.lines() {
                    let color = match text.chars().next() {
                        Some('+') => GREEN,
                        Some('-') => RED,
                        _ => DIM,
                    };
                    let _ = writeln!(out, "{BODY}{color}{text}{RESET}");
                }
            }
            let question = match ask.name.as_str() {
                "edit" => "Apply this edit?",
                "write" => "Write this file?",
                "shell" => "Run this command?",
                _ => "Run this tool?",
            };
            loop {
                let _ = write!(
                    out,
                    "\n{BODY}{BOLD}{question}{RESET}  {BOLD}y{RESET} yes  {BOLD}n{RESET} no  \
                     {BOLD}a{RESET} always this session  {for_project}{BOLD}?{RESET} why  "
                );
                let _ = out.flush();
                asking.set(true);
                let answer = loop {
                    if let Some(text) = typed.borrow_mut().pop_front() {
                        break text;
                    }
                    if ended.get() {
                        break String::new();
                    }
                    arrived.notified().await;
                };
                asking.set(false);
                match answer.trim().to_lowercase().as_str() {
                    "y" | "yes" => return true,
                    "a" | "always" => {
                        always.borrow_mut().insert(ask.name.clone());
                        return true;
                    }
                    "p" if rule.is_some() => {
                        let rule = rule.clone().expect("checked above");
                        let label = rule.label();
                        match rules.borrow_mut().add(rule) {
                            Ok(()) => {
                                let _ = writeln!(
                                    out,
                                    "{BODY}{DIM}saved: always {label} in this project{RESET}"
                                );
                            }
                            Err(e) => {
                                let _ = writeln!(
                                    out,
                                    "{BODY}{RED}could not save the rule: {e:#}{RESET}"
                                );
                            }
                        }
                        return true;
                    }
                    "?" | "why" => {
                        let why = status::why(&ask.name, ask.exposure);
                        for text in wrap(&why, width.saturating_sub(BODY.len() + 1)) {
                            let _ = writeln!(out, "{BODY}{DIM}{text}{RESET}");
                        }
                    }
                    "" | "n" | "no" => return false,
                    // A message instead of an answer declines the call and steers.
                    _ => {
                        steering.borrow_mut().push(answer.trim().to_string());
                        let _ = writeln!(
                            out,
                            "{BODY}{YELLOW}↳ declined; calx reads your message next{RESET}"
                        );
                        return false;
                    }
                }
            }
        };
        let mut steer = || std::mem::take(&mut *steering.borrow_mut());
        let (busy, interactive) = (self.busy, self.interactive);
        let mut restart = None;
        // None when the user stopped the turn.
        let outcome = {
            let run = agent.run(prompt, &mut on_event, &mut approve, &mut steer);
            tokio::pin!(run);
            let rx = self.lines.get_or_insert_with(spawn_reader);
            loop {
                tokio::select! {
                    result = &mut run => break Some(result),
                    got = rx.recv(), if !ended.get() => {
                        let Some(text) = got else {
                            ended.set(true);
                            arrived.notify_one();
                            continue;
                        };
                        // A paste is one message. No `\` here: waiting for the next
                        // line would hold up the turn.
                        let text = if interactive { gather(rx, text, false).await } else { text };
                        if !interactive || (asking.get() && !is_handoff(&text)) {
                            typed.borrow_mut().push_back(text);
                            arrived.notify_one();
                            continue;
                        }
                        if is_handoff(&text) {
                            self.queued.push_back(text);
                            continue;
                        }
                        let (mode, text) = classify(&text, busy);
                        let text = text.to_string();
                        if mode == Busy::Stop {
                            restart = (!text.is_empty()).then_some(text);
                            break None;
                        }
                        if text.is_empty() {
                            continue;
                        }
                        // The Enter that sent it ended the line on screen.
                        {
                            let mut l = line.borrow_mut();
                            *l = match *l {
                                Line::Tool { .. } | Line::Asked => Line::Asked,
                                _ => Line::Fresh,
                            };
                        }
                        if mode == Busy::Steer {
                            println!("{BODY}{YELLOW}↳ steer, at the next pause{RESET}");
                            steering.borrow_mut().push(text);
                        } else {
                            println!("{BODY}{YELLOW}↳ queued, after this turn{RESET}");
                            self.queued.push_back(text);
                        }
                    }
                }
            }
        };
        self.tally = tally.into_inner();
        self.always = always.into_inner();
        self.typed = typed.into_inner();
        let mut out = std::io::stdout();
        end_line(&mut out, &mut line.into_inner());
        // Messages the turn never read become the next prompts.
        let unread = steering.into_inner();
        match outcome {
            Some(result) => {
                for text in unread.into_iter().rev() {
                    self.queued.push_front(text);
                }
                result?;
                let _ = writeln!(out, "\n{}", self.status_line(width));
                Ok(true)
            }
            None => {
                let next: Vec<String> = unread.into_iter().chain(restart).collect();
                if !next.is_empty() {
                    self.queued.push_front(next.join("\n\n"));
                }
                let _ = writeln!(out, "\n{DIM}Stopped. This turn was not saved.{RESET}");
                Ok(false)
            }
        }
    }

    /// Context use, cache, time, and the trifecta, after each turn.
    fn status_line(&self, width: usize) -> String {
        let t = &self.tally;
        let ctx = t.last.input_tokens;
        let share = ctx * 100 / status::CONTEXT_WINDOW;
        let filled = (share as usize).div_ceil(12).min(8);
        let cache = t
            .cache_percent()
            .map(|p| format!(" · cache {p}%"))
            .unwrap_or_default();
        let bar = format!("▕{}{}▏", "█".repeat(filled), "░".repeat(8 - filled));
        let stats = format!(
            " {share}%{cache} · {} req · model {} · tools {}",
            t.turn_requests,
            status::duration(t.turn_model_ms),
            status::duration(t.turn_tool_ms),
        );
        let ctx = status::tokens(ctx);
        let used = format!("ctx {ctx} {bar}{stats}").chars().count();
        let left = format!("{DIM}ctx{RESET} {ctx} {BLUE}{bar}{RESET}{DIM}{stats}{RESET}");
        let open = status::legs(t.exposure)
            .iter()
            .filter(|(_, on)| *on)
            .count();
        let color = if open == 3 { RED } else { YELLOW };
        let legs: Vec<String> = status::legs(t.exposure)
            .iter()
            .map(|(name, on)| {
                let dot = if *on {
                    format!("{color}●{RESET}")
                } else {
                    format!("{DIM}○{RESET}")
                };
                format!("{DIM}{name}{RESET} {dot}")
            })
            .collect();
        // "untrusted ●  private ●  egress ●" is 32 columns wide. When it
        // does not fit beside the stats, it goes on its own line.
        const LEGS: usize = 32;
        let legs = legs.join("  ");
        if used + 2 + LEGS < width {
            format!("{left}{:gap$}{legs}", "", gap = width - used - LEGS - 1)
        } else {
            format!(
                "{left}\n{:gap$}{legs}",
                "",
                gap = width.saturating_sub(LEGS + 1)
            )
        }
    }
}

/// Ends a line left open by streamed text or a tool waiting for its time.
fn end_line(out: &mut std::io::Stdout, line: &mut Line) {
    match line {
        Line::Fresh => {}
        Line::Reasoning => {
            let _ = write!(out, "{RESET}\n\n");
        }
        Line::Text | Line::Tool { .. } => {
            let _ = writeln!(out);
        }
        Line::Asked => {}
    }
    *line = Line::Fresh;
}

/// Pads from column `used` so `note` ends at the right edge.
fn right(used: usize, note: &str, width: usize, color: &str) -> String {
    let gap = width.saturating_sub(used + note.chars().count() + 1).max(2);
    format!("{:gap$}{color}{note}{RESET}", "")
}

/// Keeps streamed text in the column after the margin.
fn indent(delta: &str) -> String {
    delta.replace('\n', &format!("\n{:MARGIN$}", ""))
}

fn terminal_width() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(100)
        .clamp(40, 200)
}

/// Splits text into lines of at most `max` characters at spaces.
fn wrap(text: &str, max: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    for word in text.split_whitespace() {
        let last = lines.last_mut().expect("starts with a line");
        if !last.is_empty() && last.chars().count() + 1 + word.chars().count() > max {
            lines.push(word.to_string());
        } else {
            if !last.is_empty() {
                last.push(' ');
            }
            last.push_str(word);
        }
    }
    lines
}

/// Reads stdin lines on a thread. It stops after a command that ends calx
/// or hands the terminal to the panel view, so it does not take their keys.
fn spawn_reader() -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let last = is_handoff(&line);
            if tx.send(line).is_err() || last {
                break;
            }
        }
    });
    rx
}

/// Lines of a paste reach stdin together; two presses of Enter are always
/// further apart than this.
const PASTE_GAP: std::time::Duration = std::time::Duration::from_millis(15);

/// Adds to `text` the rest of a paste it starts, and with `continuation`,
/// the next line after each that ends with `\`, as in a shell.
async fn gather(
    rx: &mut mpsc::UnboundedReceiver<String>,
    mut text: String,
    continuation: bool,
) -> String {
    loop {
        if continuation && let Some(rest) = text.strip_suffix('\\') {
            text = format!("{rest}\n");
            print!("{DIM}   …  {RESET}");
            let _ = std::io::stdout().flush();
            match rx.recv().await {
                Some(line) => text.push_str(&line),
                None => return text,
            }
            continue;
        }
        tokio::time::sleep(PASTE_GAP).await;
        let mut more = false;
        while let Ok(line) = rx.try_recv() {
            text.push('\n');
            text.push_str(&line);
            more = true;
        }
        if !more {
            return text;
        }
    }
}

/// Prints a compaction event on its own line, numbered when it added an item.
fn print_compaction(event: &Event) {
    let Some((text, error)) = status::compaction_note(event) else {
        return;
    };
    let seq = match event {
        Event::Compacted { seq, .. } => format!("§{seq}"),
        _ => String::new(),
    };
    let color = if error { RED } else { DIM };
    println!("{DIM}{seq:<MARGIN$}{RESET}{color}{text}{RESET}");
}

fn is_handoff(line: &str) -> bool {
    matches!(line.trim(), "/exit" | "/quit" | "/panels")
}

/// What a line typed during a turn does, and its text.
fn classify(line: &str, busy: Busy) -> (Busy, &str) {
    let line = line.trim();
    if line == "/stop" {
        return (Busy::Stop, "");
    }
    for (prefix, mode) in [
        ("/steer ", Busy::Steer),
        ("/queue ", Busy::Queue),
        ("/stop ", Busy::Stop),
    ] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return (mode, rest.trim());
        }
    }
    (busy, line)
}

/// Shortens text to one line of at most `max` characters for display.
pub fn one_line(text: &str, max: usize) -> String {
    let first = text.lines().next().unwrap_or_default();
    let more = text.lines().nth(1).is_some();
    let mut s: String = first.chars().take(max).collect();
    if more || first.chars().count() > max {
        s.push('…');
    }
    s
}
