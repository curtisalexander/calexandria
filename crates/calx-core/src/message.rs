//! Provider-independent conversation types. Each provider translates these to
//! and from its own wire format, so the rest of the harness never sees one.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub text: String,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            text: text.into(),
        }
    }
}

/// What a provider reports while a reply streams in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Part of the model's summary of its reasoning.
    ReasoningDelta(String),
    /// Part of the reply text.
    TextDelta(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
}
