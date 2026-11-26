//! The core of Calexandria: authentication, model providers, and the message
//! types the rest of the harness is built on. Frontends such as the `calx`
//! command-line program depend on this crate and stay thin.

pub mod agent;
pub mod auth;
pub mod message;
pub mod responses;
mod sse;
pub mod tools;

use std::path::PathBuf;

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
