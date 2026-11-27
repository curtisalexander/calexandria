//! The core of Calexandria: authentication, model providers, and the message
//! types the rest of the harness is built on. Frontends such as the `calx`
//! command-line program depend on this crate and stay thin.

pub mod agent;
pub mod approvals;
pub mod auth;
pub mod chat;
pub mod compaction;
pub mod environment;
pub mod gateway;
pub mod message;
pub mod provider;
pub mod responses;
pub mod session;
mod sse;
pub mod store;
pub mod tools;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The directory where calx keeps its state: `$CALX_HOME`, or `~/.calx`.
pub fn calx_home() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CALX_HOME") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::home_dir().context("could not find your home directory")?;
    Ok(home.join(".calx"))
}

/// A new random ID, such as for a session.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Writes a file only the current user can read (on Unix; Windows profile
/// directories are already private to the user). It writes a temporary file
/// and renames it into place, so a crash never leaves a half-written file.
pub fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&tmp)
        .with_context(|| format!("could not write {}", tmp.display()))?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path).with_context(|| format!("could not write {}", path.display()))
}

/// Creates a directory, and on Unix makes it readable only by the current user.
pub(crate) fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
