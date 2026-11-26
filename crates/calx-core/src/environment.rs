//! The execution environment: where tools act, the "hands" to the harness's
//! "brain". Every file and process operation a tool performs goes through
//! [`Environment`], so the tools never touch the OS directly.
//!
//! Today the only implementation is [`LocalEnvironment`]: this machine, as the
//! current user. A container, VM, or SSH environment can implement the same
//! trait later while the agent loop, credentials, and history stay in the calx
//! process. The trait is synchronous because tools already run on blocking
//! threads; a remote implementation can block on its own I/O.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

pub trait Environment: Send + Sync {
    /// The operating system, such as "macos", "windows", or "linux".
    fn os(&self) -> &str;
    /// The shell `exec` runs commands with, such as "bash".
    fn shell_name(&self) -> &str;
    /// The directory relative paths and commands start from.
    fn working_dir(&self) -> String;
    fn is_dir(&self, path: &str) -> bool;
    /// Entry names, sorted, with a trailing `/` on directories.
    fn list_dir(&self, path: &str) -> Result<Vec<String>>;
    fn read_file(&self, path: &str) -> Result<String>;
    /// Creates or replaces a file, creating missing parent directories.
    fn write_file(&self, path: &str, contents: &str) -> Result<()>;
    /// Runs a shell command with stdin closed, stopping it after `timeout`.
    fn exec(&self, command: &str, timeout: Duration) -> Result<ExecOutput>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    /// Stdout and stderr together, roughly interleaved.
    pub output: String,
    pub exit: Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    /// Killed by a signal (Unix).
    Signal,
    TimedOut,
}

/// Environment variables the harness uses that commands must not see.
const HIDDEN_VARS: &[&str] = &[crate::store::KEY_ENV];

/// This machine, as the current user.
pub struct LocalEnvironment {
    cwd: PathBuf,
    shell: Shell,
}

impl LocalEnvironment {
    pub fn new(cwd: PathBuf, shell: Shell) -> Self {
        Self { cwd, shell }
    }

    /// The process's working directory and the detected shell.
    pub fn current() -> Result<Arc<Self>> {
        let cwd = std::env::current_dir().context("could not find the working directory")?;
        Ok(Arc::new(Self::new(cwd, Shell::detect())))
    }

    fn resolve(&self, path: &str) -> PathBuf {
        // `join` keeps an absolute path as it is.
        self.cwd.join(path)
    }
}

impl Environment for LocalEnvironment {
    fn os(&self) -> &str {
        std::env::consts::OS
    }

    fn shell_name(&self) -> &str {
        self.shell.name()
    }

    fn working_dir(&self) -> String {
        self.cwd.display().to_string()
    }

    fn is_dir(&self, path: &str) -> bool {
        self.resolve(path).is_dir()
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>> {
        let mut names: Vec<String> = std::fs::read_dir(self.resolve(path))
            .with_context(|| format!("could not list {path}"))?
            .filter_map(|e| e.ok())
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.path().is_dir() { name + "/" } else { name }
            })
            .collect();
        names.sort();
        Ok(names)
    }

    fn read_file(&self, path: &str) -> Result<String> {
        std::fs::read_to_string(self.resolve(path))
            .with_context(|| format!("could not read {path}"))
    }

    fn write_file(&self, path: &str, contents: &str) -> Result<()> {
        let full = self.resolve(path);
        if let Some(dir) = full.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }
        std::fs::write(&full, contents).with_context(|| format!("could not write {path}"))
    }

    fn exec(&self, command: &str, timeout: Duration) -> Result<ExecOutput> {
        let mut cmd = self.shell.command(command);
        cmd.current_dir(&self.cwd);
        for var in HIDDEN_VARS {
            cmd.env_remove(var);
        }
        run(cmd, self.shell.name(), timeout)
    }
}

fn run(mut command: Command, shell_name: &str, timeout: Duration) -> Result<ExecOutput> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not start {shell_name}"))?;

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

    let output = String::from_utf8_lossy(&output.lock().unwrap()).into_owned();
    let exit = match status.map(|s| s.code()) {
        Some(Some(code)) => Exit::Code(code),
        Some(None) => Exit::Signal,
        None => Exit::TimedOut,
    };
    Ok(ExecOutput { output, exit })
}

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
        .find(|p| Path::new(p).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_env(name: &str) -> (PathBuf, LocalEnvironment) {
        let dir = std::env::temp_dir().join(format!("calx-env-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (dir.clone(), LocalEnvironment::new(dir, Shell::detect()))
    }

    #[test]
    fn relative_paths_and_commands_start_in_the_environments_directory() {
        let (dir, env) = temp_env("cwd");
        env.write_file("sub/note.txt", "hi").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("sub/note.txt")).unwrap(),
            "hi"
        );
        assert_eq!(env.read_file("sub/note.txt").unwrap(), "hi");
        assert_eq!(env.list_dir(".").unwrap(), vec!["sub/"]);
        let out = env
            .exec("cat sub/note.txt", Duration::from_secs(30))
            .unwrap();
        assert_eq!((out.output.trim(), out.exit), ("hi", Exit::Code(0)));
    }

    #[test]
    fn commands_cannot_see_the_credentials_key() {
        // SAFETY: no other test reads or writes this variable.
        unsafe { std::env::set_var(crate::store::KEY_ENV, "secret-key-value") };
        let (_, env) = temp_env("hidden");
        let out = env
            .exec("echo \"[$CALX_AUTH_KEY]\"", Duration::from_secs(30))
            .unwrap();
        assert_eq!(out.output.trim(), "[]");
    }

    #[test]
    fn stops_a_command_that_runs_too_long() {
        let (_, env) = temp_env("timeout");
        let started = Instant::now();
        let out = env.exec("sleep 20", Duration::from_secs(1)).unwrap();
        assert_eq!(out.exit, Exit::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
    }
}
