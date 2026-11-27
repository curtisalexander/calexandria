//! The shell tool: runs a command in the execution environment.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolSpec, parse_args, truncate_middle};
use crate::environment::{Environment, Exit};
use crate::message::Exposure;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_OUTPUT_BYTES: usize = 30_000;

pub struct ShellTool {
    pub env: Arc<dyn Environment>,
}

#[derive(Deserialize)]
struct ShellArgs {
    command: String,
    timeout_seconds: Option<u64>,
}

impl Tool for ShellTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "shell",
            description: format!(
                "Run a command with {} on {} in the working directory. Returns stdout and \
                 stderr combined, then the exit code. Stops the command after \
                 {DEFAULT_TIMEOUT_SECS} seconds unless timeout_seconds says otherwise. Input \
                 is closed, so interactive commands will not work.",
                self.env.shell_name(),
                self.env.os()
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout_seconds": { "type": "integer" }
                },
                "required": ["command"]
            }),
        }
    }

    fn editable(&self) -> Option<&'static str> {
        Some("command")
    }

    fn needs_approval(&self) -> bool {
        true
    }

    fn exposure(&self) -> Exposure {
        // A command can read anything, fetch anything, and send anything.
        Exposure {
            private: true,
            untrusted: true,
            egress: true,
        }
    }

    fn summary(&self, args: &Value) -> String {
        args["command"].as_str().unwrap_or("?").to_string()
    }

    fn run(&self, args: Value) -> Result<String> {
        let a: ShellArgs = parse_args(args)?;
        let timeout = Duration::from_secs(a.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1));
        let result = self.env.exec(&a.command, timeout)?;
        let mut text = truncate_middle(result.output.trim_end(), MAX_OUTPUT_BYTES);
        if !text.is_empty() {
            text.push('\n');
        }
        match result.exit {
            Exit::Code(code) => text.push_str(&format!("[exit code {code}]")),
            Exit::Signal => text.push_str("[killed by a signal]"),
            Exit::TimedOut => {
                text.push_str(&format!("[stopped after {} seconds]", timeout.as_secs()))
            }
        }
        Ok(text)
    }
}
