//! What both views report about a session: token use, timing, and which legs
//! of the lethal trifecta its tool calls have opened.

use std::collections::HashMap;

use calx_core::agent::{Agent, DECLINED, SKIPPED};
use calx_core::memory::Memory;
use calx_core::message::{Event, Exposure, Usage};
use calx_core::session::Sessions;

/// The context window the views measure against when neither the user nor
/// the server says what it is.
pub const ASSUMED_WINDOW: u64 = 400_000;

/// The model's context window, and where calx learned it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub tokens: u64,
    pub from: WindowFrom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFrom {
    /// --context-window or CALX_CONTEXT_WINDOW.
    Flag,
    /// Set in Prefs, and maybe saved for this server and model.
    Prefs,
    Server,
}

#[derive(Default)]
pub struct Tally {
    /// The latest request's usage. Its input is how big the context is now.
    pub last: Usage,
    pub session: Usage,
    pub session_requests: usize,
    pub turn_requests: usize,
    pub turn_model_ms: u64,
    pub turn_tool_ms: u64,
    /// Input tokens of each request this run, oldest first.
    pub history: Vec<u64>,
    /// The legs opened by tool calls that ran, in this session so far.
    pub exposure: Exposure,
    /// The model's context window, when known.
    pub window: Option<Window>,
    started: HashMap<usize, Exposure>,
}

impl Tally {
    /// Starts from the tool calls already in a resumed session.
    pub fn new(agent: &Agent) -> Self {
        Self {
            exposure: agent.opened(),
            ..Self::default()
        }
    }

    pub fn start_turn(&mut self) {
        self.turn_requests = 0;
        self.turn_model_ms = 0;
        self.turn_tool_ms = 0;
    }

    pub fn observe(&mut self, event: &Event) {
        match event {
            Event::RequestFinished { usage, duration_ms } => {
                self.last = *usage;
                self.session += *usage;
                self.session_requests += 1;
                self.turn_requests += 1;
                self.turn_model_ms += duration_ms;
                self.history.push(usage.input_tokens);
            }
            // The compaction requests were the biggest yet; the context is
            // what is left, until the next request counts it.
            Event::Compacted { after, .. } => {
                self.last = Usage {
                    input_tokens: *after,
                    ..Usage::default()
                };
            }
            Event::ToolStarted { seq, exposure, .. } => {
                self.started.insert(*seq, *exposure);
            }
            Event::ToolFinished {
                seq,
                output,
                duration_ms,
                ..
            } => {
                self.turn_tool_ms += duration_ms;
                // A declined or skipped call opened nothing.
                if let Some(exposure) = self.started.remove(seq)
                    && ran(output)
                {
                    self.exposure |= exposure;
                }
            }
            _ => {}
        }
    }

    /// The context window to measure against.
    pub fn window(&self) -> u64 {
        self.window.map_or(ASSUMED_WINDOW, |w| w.tokens)
    }

    /// The window for the user, such as "128k (from the server)".
    pub fn window_label(&self) -> String {
        let from = match self.window.map(|w| w.from) {
            Some(WindowFrom::Flag) => "--context-window",
            Some(WindowFrom::Prefs) => "set in Prefs",
            Some(WindowFrom::Server) => "from the server",
            None => "assumed",
        };
        format!("{} ({from})", tokens(self.window()))
    }

    /// The share of the latest request's input the server had cached.
    pub fn cache_percent(&self) -> Option<u64> {
        (self.last.input_tokens > 0)
            .then(|| self.last.cached_input_tokens * 100 / self.last.input_tokens)
    }
}

/// Whether a tool call with this result ran, rather than being declined or skipped.
pub fn ran(output: &str) -> bool {
    output != DECLINED && output != SKIPPED
}

/// What a prompt sent while a turn runs does.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    clap::ValueEnum,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Busy {
    /// Read by the model at the next pause in the turn.
    #[default]
    Steer,
    /// Sent as a new prompt once the turn ends.
    Queue,
    /// Stops the turn and sends it as a new prompt.
    Stop,
}

impl Busy {
    pub fn name(self) -> &'static str {
        match self {
            Busy::Steer => "steer",
            Busy::Queue => "queue",
            Busy::Stop => "stop",
        }
    }
}

/// The three legs, in the order the views show them.
pub fn legs(e: Exposure) -> [(&'static str, bool); 3] {
    [
        ("untrusted", e.untrusted),
        ("private", e.private),
        ("egress", e.egress),
    ]
}

/// Why a tool asks before it runs, for the `?` answer.
pub fn why(name: &str, e: Exposure) -> String {
    let mut opens = Vec::new();
    if e.private {
        opens.push("reach your private files");
    }
    if e.untrusted {
        opens.push("bring in content someone else wrote");
    }
    if e.egress {
        opens.push("send data off this machine");
    }
    let opens = match opens.as_slice() {
        [] => "change this machine".to_string(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    };
    let all = if e.private && e.untrusted && e.egress {
        " That is all three legs of the lethal trifecta: text it reads could tell the model to send your data out."
    } else {
        ""
    };
    format!("{name} changes things and can {opens}.{all}")
}

/// What a compaction event says, for both views, and whether it is an error.
pub fn compaction_note(event: &Event) -> Option<(String, bool)> {
    Some(match event {
        Event::CompactionStarted { tokens: n } => (
            format!("compacting the conversation, ~{} tokens…", tokens(*n)),
            false,
        ),
        Event::Compacted {
            before,
            after,
            trims,
            squishes,
            attempts,
            ..
        } => (
            format!(
                "compacted ~{} → ~{} tokens: {trims} trimmed, {squishes} squished, {attempts} {}",
                tokens(*before),
                tokens(*after),
                if *attempts == 1 { "try" } else { "tries" }
            ),
            false,
        ),
        Event::CompactionFailed { reason } => (format!("could not compact: {reason}"), true),
        _ => return None,
    })
}

/// What `/remember`, `/remember-here`, `/forget N`, and `/memories` do,
/// in either view: `None` when `line` is not one of them, else what to show
/// and whether it is an error.
pub fn memory_command(line: &str, project: &str, session_id: &str) -> Option<(String, bool)> {
    let (command, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    let result = (|| -> anyhow::Result<String> {
        let db = Sessions::open()?;
        Ok(match command {
            "/remember" | "/remember-here" => {
                let here = (command == "/remember-here").then_some(project);
                let id = db.remember(rest, here, Some(session_id), false)?;
                format!(
                    "remembered as memory {id}, for {}; it is in the instructions from the next session on",
                    if here.is_some() {
                        "this project"
                    } else {
                        "you, everywhere"
                    }
                )
            }
            "/forget" => {
                let id = rest.parse().map_err(|_| {
                    anyhow::anyhow!("/forget takes a memory's number, from /memories")
                })?;
                format!("forgot memory {id}: {}", db.forget(id)?.text)
            }
            "/memories" => {
                let all = db.memories(Some(project))?;
                if all.is_empty() {
                    "no memories yet; /remember TEXT saves one, and calx may suggest some".into()
                } else {
                    all.iter().map(memory_line).collect::<Vec<_>>().join("\n")
                }
            }
            _ => unreachable!(),
        })
    })();
    if !matches!(
        command,
        "/remember" | "/remember-here" | "/forget" | "/memories"
    ) {
        return None;
    }
    Some(match result {
        Ok(text) => (text, false),
        Err(e) => (format!("{e:#}"), true),
    })
}

/// One memory, as both views and `calx memory` list it.
pub fn memory_line(m: &Memory) -> String {
    format!(
        "{:>3}  {:<12} {}{}",
        m.id,
        if m.project.is_some() {
            "this project"
        } else {
            "you"
        },
        m.text.replace('\n', " "),
        if m.untrusted {
            "  (saved after reading untrusted content)"
        } else {
            ""
        }
    )
}

/// A token count for display, such as 950, 31.2k, or 400k.
pub fn tokens(n: u64) -> String {
    match n {
        0..1000 => n.to_string(),
        1000..100_000 => format!("{:.1}k", n as f64 / 1000.0),
        _ => format!("{}k", n / 1000),
    }
}

/// A duration for display, such as 2 ms, 1.8s, 41s, or 2m 05s.
pub fn duration(ms: u64) -> String {
    match ms {
        0..1000 => format!("{ms} ms"),
        1000..10_000 => format!("{:.1}s", ms as f64 / 1000.0),
        10_000..60_000 => format!("{}s", ms / 1000),
        _ => format!("{}m {:02}s", ms / 60_000, ms / 1000 % 60),
    }
}

/// Rough tokens in `text`: about four characters each.
pub fn estimate(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(seq: usize) -> Event {
        Event::ToolStarted {
            seq,
            name: "shell".into(),
            summary: String::new(),
            exposure: Exposure {
                private: true,
                untrusted: true,
                egress: true,
            },
        }
    }

    fn finished(seq: usize, output: &str) -> Event {
        Event::ToolFinished {
            seq,
            name: "shell".into(),
            output: output.into(),
            is_error: true,
            duration_ms: 3,
        }
    }

    #[test]
    fn only_calls_that_ran_open_trifecta_legs() {
        let mut tally = Tally::default();
        tally.observe(&started(4));
        tally.observe(&finished(4, DECLINED));
        assert_eq!(tally.exposure, Exposure::default());
        // A failed command still ran, so it counts.
        tally.observe(&started(6));
        tally.observe(&finished(6, "Error: exit 1"));
        assert!(tally.exposure.egress && tally.exposure.private && tally.exposure.untrusted);
    }
}
