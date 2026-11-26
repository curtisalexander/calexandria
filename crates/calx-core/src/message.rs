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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Part of the model's summary of its reasoning.
    ReasoningDelta(String),
    /// Part of the reply text.
    TextDelta(String),
    /// A tool is about to run; `summary` is one line such as the command or path.
    ToolStarted { name: String, summary: String },
    ToolFinished {
        name: String,
        output: String,
        is_error: bool,
    },
    /// Tokens used by one model request.
    Usage(Usage),
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
