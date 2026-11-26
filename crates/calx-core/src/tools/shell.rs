//! The shell tool: bash where there is one (Git Bash on Windows), otherwise PowerShell.

use std::io::Read as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolSpec, parse_args, truncate_middle};

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_OUTPUT_BYTES: usize = 30_000;

#[derive(Debug, Clone)]
pub enum Shell {
    Bash(PathBuf),
    PowerShell(PathBuf),
}

impl Shell {
    /// `CALX_SHELL` names a shell by path. Otherwise: bash, which on Windows
    /// means Git Bash, then PowerShell on Windows or `sh` elsewhere.
    pub fn detect() -> Self {
        if let Some(path) = std::env::var_os("CALX_SHELL") {
            let path = PathBuf::from(path);
            let stem = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase();
            return if stem.contains("pwsh") || stem.contains("powershell") {
                Self::PowerShell(path)
            } else {
                Self::Bash(path)
            };
        }
        if cfg!(windows) {
            if let Some(bash) = git_bash() {
                return Self::Bash(bash);
            }
            let ps = find_on_path("pwsh")
                .or_else(|| find_on_path("powershell"))
                .unwrap_or_else(|| PathBuf::from("powershell.exe"));
            return Self::PowerShell(ps);
        }
        Self::Bash(find_on_path("bash").unwrap_or_else(|| PathBuf::from("/bin/sh")))
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Bash(_) => "bash",
            Self::PowerShell(_) => "PowerShell",
        }
    }

    fn command(&self, script: &str) -> Command {
        match self {
            Self::Bash(path) => {
                let mut c = Command::new(path);
                c.arg("-c").arg(script);
                c
            }
            Self::PowerShell(path) => {
                let mut c = Command::new(path);
                c.args(["-NoProfile", "-NonInteractive", "-Command", script]);
                c
            }
        }
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(&file))
        .find(|p| p.is_file())
}

/// Git Bash, found next to `git`. Plain `bash` on a Windows PATH is often
/// WSL's launcher in System32, which runs in a different file system.
fn git_bash() -> Option<PathBuf> {
    // git.exe lives in <Git>\cmd, and bash.exe in <Git>\bin.
    let near_git = find_on_path("git")
        .and_then(|git| Some(git.parent()?.parent()?.join("bin").join("bash.exe")));
    near_git
        .into_iter()
        .chain([PathBuf::from(r"C:\Program Files\Git\bin\bash.exe")])
        .find(|p| p.is_file())
}

pub struct ShellTool {
    shell: Shell,
}

impl ShellTool {
    pub fn new(shell: Shell) -> Self {
        Self { shell }
    }
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
                self.shell.name(),
                std::env::consts::OS
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

    fn needs_approval(&self) -> bool {
        true
    }

    fn summary(&self, args: &Value) -> String {
        args["command"].as_str().unwrap_or("?").to_string()
    }

    fn run(&self, args: Value) -> Result<String> {
        let a: ShellArgs = parse_args(args)?;
        let timeout = Duration::from_secs(a.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECS).max(1));
        run_command(&self.shell, &a.command, timeout)
    }
}

fn run_command(shell: &Shell, script: &str, timeout: Duration) -> Result<String> {
    let mut child = shell
        .command(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not start {}", shell.name()))?;

    // Both pipes feed one buffer, so stdout and stderr stay roughly interleaved.
    let output = Arc::new(Mutex::new(Vec::new()));
    let (done_tx, done_rx) = mpsc::channel();
    let pipes: [Option<Box<dyn std::io::Read + Send>>; 2] = [
        child.stdout.take().map(|p| Box::new(p) as _),
        child.stderr.take().map(|p| Box::new(p) as _),
    ];
    for mut pipe in pipes.into_iter().flatten() {
        let output = output.clone();
        let done = done_tx.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n @ 1..) = pipe.read(&mut buf) {
                output.lock().unwrap().extend_from_slice(&buf[..n]);
            }
            let _ = done.send(());
        });
    }
    drop(done_tx);

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    // A background process the command started can hold the pipes open
    // forever, so wait only briefly for the last of the output.
    let grace = Instant::now() + Duration::from_secs(1);
    for _ in 0..2 {
        if done_rx
            .recv_timeout(grace.saturating_duration_since(Instant::now()))
            .is_err()
        {
            break;
        }
    }

    let text = String::from_utf8_lossy(&output.lock().unwrap()).into_owned();
    let mut text = truncate_middle(text.trim_end(), MAX_OUTPUT_BYTES);
    if !text.is_empty() {
        text.push('\n');
    }
    match status.map(|s| s.code()) {
        Some(Some(code)) => text.push_str(&format!("[exit code {code}]")),
        Some(None) => text.push_str("[killed by a signal]"),
        None => text.push_str(&format!("[stopped after {} seconds]", timeout.as_secs())),
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_a_command_and_reports_the_exit_code() {
        let out = run_command(&Shell::detect(), "echo hello", Duration::from_secs(30)).unwrap();
        assert_eq!(out.replace("\r\n", "\n"), "hello\n[exit code 0]");
    }

    #[test]
    fn stops_a_command_that_runs_too_long() {
        let started = Instant::now();
        let out = run_command(&Shell::detect(), "sleep 20", Duration::from_secs(1)).unwrap();
        assert!(out.ends_with("[stopped after 1 seconds]"), "{out}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
    }
}
