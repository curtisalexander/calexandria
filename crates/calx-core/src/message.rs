//! Provider-independent conversation types. Each provider translates these to
//! and from its own wire format, so the rest of the harness never sees one.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One entry in a conversation, in order.
///
/// `raw` holds the item exactly as the provider returned it, for providers
/// that need their own items sent back, such as OpenAI's encrypted reasoning.
/// Only the provider that produced it reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Item {
    User {
        text: String,
    },
    Assistant {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw: Option<Value>,
    },
    /// The model's summary of its reasoning.
    Reasoning {
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw: Option<Value>,
    },
    ToolCall {
        call: ToolCall,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw: Option<Value>,
    },
    ToolResult {
        call_id: String,
        output: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// The arguments as the JSON text the model wrote.
    pub arguments: String,
}

/// What the agent reports while it works.
///
/// `seq` numbers an item by its place in the conversation, counting from 1,
/// the same number the session store gives it. The UI shows it as `§seq`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A model request is about to be sent with the first `items` items.
    RequestStarted { step: usize, items: usize },
    /// Part of the model's summary of its reasoning.
    ReasoningDelta(String),
    /// Part of the reply text.
    TextDelta(String),
    /// A model request finished.
    RequestFinished { usage: Usage, duration_ms: u64 },
    /// A tool call is starting, before any approval; `seq` is the call's
    /// item. `summary` is one line such as the command or path.
    ToolStarted {
        seq: usize,
        name: String,
        summary: String,
        exposure: Exposure,
    },
    /// A message the user sent during the turn was added as item `seq`.
    Steered { seq: usize, text: String },
    /// A tool finished, or was declined or skipped; `seq` is its call's, as in `ToolStarted`.
    ToolFinished {
        seq: usize,
        name: String,
        output: String,
        is_error: bool,
        duration_ms: u64,
    },
}

/// Which legs of the lethal trifecta a tool opens: access to private data,
/// exposure to untrusted content, and a way to send data out. An agent with
/// all three can be tricked into leaking what it can read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Exposure {
    pub private: bool,
    pub untrusted: bool,
    pub egress: bool,
}

impl std::ops::BitOrAssign for Exposure {
    fn bitor_assign(&mut self, other: Self) {
        self.private |= other.private;
        self.untrusted |= other.untrusted;
        self.egress |= other.egress;
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, other: Self) {
        self.input_tokens += other.input_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.output_tokens += other.output_tokens;
    }
}
