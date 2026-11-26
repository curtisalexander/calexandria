//! What both views report about a session: token use, timing, and which legs
//! of the lethal trifecta its tool calls have opened.

use std::collections::HashMap;

use calx_core::agent::{Agent, DECLINED};
use calx_core::message::{Event, Exposure, Item, Usage};

/// The context window the views measure use against. Responses does not
/// report a model's window, so this is an assumption to revisit per model.
pub const CONTEXT_WINDOW: u64 = 400_000;

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
    started: HashMap<usize, Exposure>,
}

impl Tally {
    /// Starts from the tool calls already in a resumed session.
    pub fn new(agent: &Agent) -> Self {
        let mut declined = std::collections::HashSet::new();
        for item in &agent.items {
            if let Item::ToolResult { call_id, output } = item
                && output == DECLINED
            {
                declined.insert(call_id.as_str());
            }
        }
        let mut exposure = Exposure::default();
        for item in &agent.items {
            if let Item::ToolCall { call, .. } = item
                && !declined.contains(call.id.as_str())
                && let Some(tool) = agent.toolbox.get(&call.name)
            {
                exposure |= tool.exposure();
            }
        }
        Self {
            exposure,
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
                // A declined call opened nothing.
                if let Some(exposure) = self.started.remove(seq)
                    && output != DECLINED
                {
                    self.exposure |= exposure;
                }
            }
            _ => {}
        }
    }

    /// The share of the latest request's input the server had cached.
    pub fn cache_percent(&self) -> Option<u64> {
        (self.last.input_tokens > 0)
            .then(|| self.last.cached_input_tokens * 100 / self.last.input_tokens)
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
