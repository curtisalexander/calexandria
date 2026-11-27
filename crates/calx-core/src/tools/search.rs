//! Search tools: grep and find, on ripgrep's own crates, through the
//! execution environment. They only read, so they run without asking.

use std::fmt::Write as _;
use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolSpec, parse_args, truncate_middle};
use crate::environment::{Environment, GrepQuery};
use crate::message::Exposure;

const MAX_OUTPUT_BYTES: usize = 50_000;
const GREP_LIMIT: usize = 200;
const FIND_LIMIT: usize = 500;

/// Reading file names and contents: private, and possibly written by someone else.
const READS: Exposure = Exposure {
    private: true,
    untrusted: true,
    egress: false,
};

fn in_path(args: &Value) -> String {
    match args["path"].as_str() {
        Some(p) if p != "." && !p.is_empty() => format!(" in {p}"),
        _ => String::new(),
    }
}

fn glob_note(args: &Value) -> String {
    args["glob"]
        .as_str()
        .map(|g| format!(" ({g})"))
        .unwrap_or_default()
}

pub struct Grep {
    pub env: Arc<dyn Environment>,
}

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    literal: bool,
    #[serde(default)]
    limit: Option<usize>,
}

impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep",
            description: format!(
                "Search file contents with a regular expression (Rust regex syntax, as in \
                 ripgrep). Skips hidden, binary, and .gitignored files. Returns matching \
                 lines grouped by file, with line numbers, at most {GREP_LIMIT} by default. \
                 Use this instead of running grep or rg in the shell."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string", "description": "Directory or file to search. Default: the working directory." },
                    "glob": { "type": "string", "description": "Only files matching this, such as *.rs or src/**/*.ts." },
                    "ignore_case": { "type": "boolean" },
                    "literal": { "type": "boolean", "description": "Match the pattern as plain text." },
                    "limit": { "type": "integer", "description": "Most matching lines to return." }
                },
                "required": ["pattern"]
            }),
        }
    }

    fn needs_approval(&self) -> bool {
        false
    }

    fn exposure(&self) -> Exposure {
        READS
    }

    fn summary(&self, args: &Value) -> String {
        let pattern = args["pattern"].as_str().unwrap_or("?");
        format!("{pattern}{}{}", in_path(args), glob_note(args))
    }

    fn run(&self, args: Value) -> Result<String> {
        let a: GrepArgs = parse_args(args)?;
        let limit = a.limit.unwrap_or(GREP_LIMIT).clamp(1, 5 * GREP_LIMIT);
        let query = GrepQuery {
            pattern: a.pattern,
            path: a.path.unwrap_or_else(|| ".".into()),
            glob: a.glob,
            ignore_case: a.ignore_case,
            literal: a.literal,
        };
        let found = self.env.grep(&query, limit)?;
        if found.items.is_empty() {
            return Ok("No matches.".into());
        }
        let mut out = String::new();
        let mut file = "";
        for m in &found.items {
            if m.path != file {
                if !file.is_empty() {
                    out.push('\n');
                }
                file = &m.path;
                let _ = writeln!(out, "{file}");
            }
            let _ = writeln!(out, "{}: {}", m.line, m.text);
        }
        if found.more {
            let _ = writeln!(
                out,
                "\n[stopped at {limit} matching lines; narrow the pattern, path, or glob]"
            );
        }
        Ok(truncate_middle(&out, MAX_OUTPUT_BYTES))
    }
}

pub struct Find {
    pub env: Arc<dyn Environment>,
}

#[derive(Deserialize)]
struct FindArgs {
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

impl Tool for Find {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "find",
            description: format!(
                "List files, recursively, as fd does: skips hidden and .gitignored files. \
                 A glob without a / matches file names (*.rs, Cargo.toml); one with a / \
                 matches paths (src/**/mod.rs). Returns sorted paths, at most {FIND_LIMIT} \
                 by default. Use this instead of find or ls -R in the shell."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "glob": { "type": "string" },
                    "path": { "type": "string", "description": "Directory to search. Default: the working directory." },
                    "limit": { "type": "integer", "description": "Most paths to return." }
                }
            }),
        }
    }

    fn needs_approval(&self) -> bool {
        false
    }

    fn exposure(&self) -> Exposure {
        READS
    }

    fn summary(&self, args: &Value) -> String {
        let glob = args["glob"].as_str().unwrap_or("*");
        format!("{glob}{}", in_path(args))
    }

    fn run(&self, args: Value) -> Result<String> {
        let a: FindArgs = parse_args(args)?;
        let limit = a.limit.unwrap_or(FIND_LIMIT).clamp(1, 4 * FIND_LIMIT);
        let path = a.path.unwrap_or_else(|| ".".into());
        let found = self.env.find(&path, a.glob.as_deref(), limit)?;
        if found.items.is_empty() {
            return Ok("No files found.".into());
        }
        let mut out = found.items.join("\n");
        if found.more {
            let _ = write!(
                out,
                "\n\n[stopped at {limit} files; narrow the glob or path]"
            );
        }
        Ok(truncate_middle(&out, MAX_OUTPUT_BYTES))
    }
}
