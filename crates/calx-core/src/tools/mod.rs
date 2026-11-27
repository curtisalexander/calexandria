//! Tools the model can call.
//!
//! A tool runs synchronously on a blocking thread: file and process work is
//! blocking anyway, and it keeps `Tool` usable as a trait object. Tools never
//! touch the OS directly; they act through an [`Environment`].

mod files;
mod search;
mod shell;

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::environment::Environment;
use crate::message::Exposure;

/// What the model is told about a tool.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: String,
    /// A JSON Schema for the arguments.
    pub parameters: Value,
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// Whether to ask the user before running it.
    fn needs_approval(&self) -> bool;
    /// One line for the user, such as the path or the command.
    fn summary(&self, args: &Value) -> String;
    /// What the call would change, shown to the user before they approve it,
    /// such as a diff.
    fn preview(&self, _args: &Value) -> Option<String> {
        None
    }
    /// Which legs of the lethal trifecta this tool opens.
    fn exposure(&self) -> Exposure {
        Exposure::default()
    }
    fn run(&self, args: Value) -> Result<String>;
}

/// The tools available to one agent.
#[derive(Clone)]
pub struct Toolbox {
    tools: Vec<Arc<dyn Tool>>,
}

impl Toolbox {
    /// The built-in tools: read, grep, find, write, edit, and shell, all acting in `env`.
    pub fn builtin(env: Arc<dyn Environment>) -> Self {
        Self {
            tools: vec![
                Arc::new(files::Read { env: env.clone() }),
                Arc::new(search::Grep { env: env.clone() }),
                Arc::new(search::Find { env: env.clone() }),
                Arc::new(files::Write { env: env.clone() }),
                Arc::new(files::Edit { env: env.clone() }),
                Arc::new(shell::ShellTool { env }),
            ],
        }
    }

    /// Adds tools, such as the memory tools for a session.
    pub fn extend(&mut self, tools: Vec<Arc<dyn Tool>>) {
        self.tools.extend(tools);
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.spec().name == name).cloned()
    }
}

/// Parses a tool's JSON arguments into its argument struct.
pub(crate) fn parse_args<T: serde::de::DeserializeOwned>(args: Value) -> Result<T> {
    serde_json::from_value(args).map_err(|e| anyhow::anyhow!("invalid arguments: {e}"))
}

/// Caps tool output at about `max` bytes: the first third and the last two
/// thirds, since errors and summaries usually come at the end.
pub(crate) fn truncate_middle(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let head_end = text.floor_char_boundary(max / 3);
    let tail_start = text.ceil_char_boundary(text.len() - (max - max / 3));
    format!(
        "{}\n[... {} bytes omitted ...]\n{}",
        &text[..head_end],
        tail_start - head_end,
        &text[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_middle_keeps_both_ends_on_char_boundaries() {
        // 3-byte characters, so byte cut points fall inside characters.
        let text = "€".repeat(100);
        let out = truncate_middle(&text, 32);
        let (head, rest) = out.split_once("\n[... ").unwrap();
        let (omitted, tail) = rest.split_once(" bytes omitted ...]\n").unwrap();
        assert_eq!(head, "€".repeat(3)); // 10 bytes rounds down to 3 characters
        assert_eq!(tail, "€".repeat(7)); // 22 bytes rounds down to 7 characters
        assert_eq!(omitted.parse::<usize>().unwrap(), 300 - 9 - 21);
        assert_eq!(truncate_middle("short", 32), "short");
    }
}
