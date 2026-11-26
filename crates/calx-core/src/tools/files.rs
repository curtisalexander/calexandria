//! File tools: read, write, and edit, done through the execution environment.
//! Paths are relative to its working directory.

use std::fmt::Write as _;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolSpec, parse_args, truncate_middle};
use crate::environment::Environment;

const MAX_READ_LINES: usize = 2000;
const MAX_OUTPUT_BYTES: usize = 50_000;

fn path_summary(args: &Value) -> String {
    args["path"].as_str().unwrap_or("?").to_string()
}

pub struct Read {
    pub env: Arc<dyn Environment>,
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

impl Tool for Read {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read",
            description: format!(
                "Read a text file, with line numbers, or list a directory. Shows at most \
                 {MAX_READ_LINES} lines at a time; use offset and limit for longer files."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File or directory path." },
                    "offset": { "type": "integer", "description": "First line to show, from 1." },
                    "limit": { "type": "integer", "description": "How many lines to show." }
                },
                "required": ["path"]
            }),
        }
    }

    fn needs_approval(&self) -> bool {
        false
    }

    fn summary(&self, args: &Value) -> String {
        path_summary(args)
    }

    fn run(&self, args: Value) -> Result<String> {
        let a: ReadArgs = parse_args(args)?;
        if self.env.is_dir(&a.path) {
            let names = self.env.list_dir(&a.path)?;
            return Ok(if names.is_empty() {
                "(empty directory)".into()
            } else {
                names.join("\n")
            });
        }
        let text = self.env.read_file(&a.path)?;
        let lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() {
            return Ok("(empty file)".into());
        }
        let start = a.offset.unwrap_or(1).max(1);
        if start > lines.len() {
            bail!(
                "offset {start} is past the end of the file, which has {} lines",
                lines.len()
            );
        }
        let limit = a.limit.unwrap_or(MAX_READ_LINES).clamp(1, MAX_READ_LINES);
        let end = (start - 1 + limit).min(lines.len());
        let mut out = String::new();
        for (i, line) in lines[start - 1..end].iter().enumerate() {
            let _ = writeln!(out, "{}\t{line}", start + i);
        }
        if end < lines.len() {
            let _ = writeln!(
                out,
                "[{} more lines; continue with offset {}]",
                lines.len() - end,
                end + 1
            );
        }
        Ok(truncate_middle(&out, MAX_OUTPUT_BYTES))
    }
}

pub struct Write {
    pub env: Arc<dyn Environment>,
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

impl Tool for Write {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write",
            description: "Create a file, or replace a file's whole contents. Creates missing \
                          directories. To change part of a file, use edit instead."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn needs_approval(&self) -> bool {
        true
    }

    fn summary(&self, args: &Value) -> String {
        path_summary(args)
    }

    fn run(&self, args: Value) -> Result<String> {
        let a: WriteArgs = parse_args(args)?;
        self.env.write_file(&a.path, &a.content)?;
        Ok(format!("Wrote {} bytes to {}", a.content.len(), a.path))
    }
}

pub struct Edit {
    pub env: Arc<dyn Environment>,
}

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_text: String,
    new_text: String,
}

impl Tool for Edit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit",
            description: "Replace one exact piece of text in a file. old_text must appear \
                          exactly once, so include enough surrounding lines to make it unique."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_text": { "type": "string", "description": "Text to replace, copied exactly from the file." },
                    "new_text": { "type": "string" }
                },
                "required": ["path", "old_text", "new_text"]
            }),
        }
    }

    fn needs_approval(&self) -> bool {
        true
    }

    fn summary(&self, args: &Value) -> String {
        path_summary(args)
    }

    fn run(&self, args: Value) -> Result<String> {
        let a: EditArgs = parse_args(args)?;
        let text = self.env.read_file(&a.path)?;
        let updated = replace_once(&text, &a.old_text, &a.new_text)
            .with_context(|| format!("could not edit {}", a.path))?;
        self.env.write_file(&a.path, &updated)?;
        Ok(format!("Edited {}", a.path))
    }
}

/// Replaces the single occurrence of `old` in `text`. Models write `\n`, so in
/// a file with Windows line endings the texts are matched with `\r\n` instead.
fn replace_once(text: &str, old: &str, new: &str) -> Result<String> {
    if old.is_empty() {
        bail!("old_text is empty");
    }
    let (old, new) = if !text.contains(old) && text.contains("\r\n") {
        let crlf = |s: &str| s.replace("\r\n", "\n").replace('\n', "\r\n");
        (crlf(old), crlf(new))
    } else {
        (old.to_string(), new.to_string())
    };
    match text.matches(&old).count() {
        0 => bail!("old_text was not found; read the file and copy the text exactly"),
        1 => Ok(text.replacen(&old, &new, 1)),
        n => bail!("old_text appears {n} times; include more surrounding lines so it matches once"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_once_matches_lf_text_in_a_crlf_file() {
        let text = "a\r\nb\r\nc\r\n";
        assert_eq!(
            replace_once(text, "b\nc", "B\nC").unwrap(),
            "a\r\nB\r\nC\r\n"
        );
    }

    #[test]
    fn replace_once_rejects_missing_and_repeated_text() {
        assert!(
            replace_once("x y x", "z", "w")
                .unwrap_err()
                .to_string()
                .contains("not found")
        );
        assert!(
            replace_once("x y x", "x", "w")
                .unwrap_err()
                .to_string()
                .contains("2 times")
        );
        assert_eq!(replace_once("x y x", "x y", "w").unwrap(), "w x");
    }
}
