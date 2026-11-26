//! Marginalia, the simple view: plain scrolling output that works in any
//! terminal, over SSH, and in scrollback. The left margin numbers each item
//! (`§16`) and the right margin notes how long a tool took.

use std::cell::RefCell;
use std::collections::HashSet;
use std::io::{BufRead, Write};

use anyhow::Result;
use calx_core::agent::{Agent, Approval, DECLINED};
use calx_core::message::Event;

use crate::status::{self, Tally};

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
    pub tally: Tally,
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
    /// Reads prompts and runs them until the user quits.
    pub async fn converse(
        &mut self,
        agent: &mut Agent,
        save: &dyn Fn(&Agent, usize),
    ) -> Result<Exit> {
        println!(
            "{DIM}calx · {} · session {} · /panels for panels · /exit to quit{RESET}",
            agent.model,
            &agent.session_id[..8]
        );
        loop {
            print!("\n{DIM}§{:<4}{RESET}{BOLD}>{RESET} ", agent.items.len() + 1);
            std::io::stdout().flush()?;
            let Some(line) = read_line() else {
                return Ok(Exit::Quit);
            };
            match line.trim() {
                "" => continue,
                "/exit" | "/quit" => return Ok(Exit::Quit),
                "/panels" => return Ok(Exit::Panels),
                prompt => {
                    let before = agent.items.len();
                    match self.turn(agent, prompt).await {
                        Ok(()) => save(agent, before),
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
    pub async fn turn(&mut self, agent: &mut Agent, prompt: &str) -> Result<()> {
        self.tally.start_turn();
        let width = terminal_width();
        let line = RefCell::new(Line::Fresh);
        let tally = RefCell::new(std::mem::take(&mut self.tally));
        let (yes, always) = (self.yes, RefCell::new(std::mem::take(&mut self.always)));

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
                Event::RequestStarted { .. } | Event::RequestFinished { .. } => {}
            }
            let _ = out.flush();
        };
        let mut approve = async |ask: Approval| -> bool {
            if yes || always.borrow().contains(&ask.name) {
                return true;
            }
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
                     {BOLD}a{RESET} always this session  {BOLD}?{RESET} why  "
                );
                let _ = out.flush();
                let answer = read_line().unwrap_or_default();
                match answer.trim().to_lowercase().as_str() {
                    "y" | "yes" => return true,
                    "a" | "always" => {
                        always.borrow_mut().insert(ask.name.clone());
                        return true;
                    }
                    "?" | "why" => {
                        let why = status::why(&ask.name, ask.exposure);
                        for text in wrap(&why, width.saturating_sub(BODY.len() + 1)) {
                            let _ = writeln!(out, "{BODY}{DIM}{text}{RESET}");
                        }
                    }
                    _ => return false,
                }
            }
        };
        let result = agent.run(prompt, &mut on_event, &mut approve).await;
        self.tally = tally.into_inner();
        self.always = always.into_inner();
        let mut out = std::io::stdout();
        end_line(&mut out, &mut line.into_inner());
        result?;
        let _ = writeln!(out, "\n{}", self.status_line(width));
        Ok(())
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

/// Reads a line from stdin; `None` at end of input.
pub fn read_line() -> Option<String> {
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(n) if n > 0 => Some(line),
        _ => None,
    }
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
