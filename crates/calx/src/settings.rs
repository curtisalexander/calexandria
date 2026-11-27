//! Defaults saved from the Prefs panel, in `settings.json` in the calx home.
//! A flag or `CALX_*` variable overrides a saved default, which overrides
//! calx's own.
//!
//! The model and effort are saved per server, so a model chosen for a local
//! server is never sent to another. Approvals are never saved: running tools
//! without asking stays a choice made for one session.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::editor::Keys;
use crate::status::Busy;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub while_busy: Option<Busy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_keys: Option<Keys>,
    /// 0 means never.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    /// By [`server_key`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub servers: BTreeMap<String, Server>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Server {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// "none" when the server is sent no effort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Context windows set in Prefs, by model.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub windows: BTreeMap<String, u64>,
}

/// Names one server for the saved model and effort, such as
/// "responses https://api.openai.com/v1".
pub fn server_key(api: &str, base_url: &str) -> String {
    format!("{api} {}", base_url.trim_end_matches('/'))
}

impl Settings {
    fn path() -> Result<std::path::PathBuf> {
        Ok(calx_core::calx_home()?.join("settings.json"))
    }

    /// The saved defaults, or none when nothing is saved yet.
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("{} is not valid; fix or delete it", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<std::path::PathBuf> {
        let path = Self::path()?;
        calx_core::write_private(&path, &serde_json::to_string_pretty(self)?)?;
        Ok(path)
    }
}

/// Reads a context window for clap, such as 128k.
pub fn parse_window(text: &str) -> Result<u64, String> {
    match parse_tokens(text) {
        Some(Some(n)) => Ok(n),
        _ => Err("expected a token count, such as 128k or 32768".into()),
    }
}

/// Reads a token count such as 200000, 200k, or 1.5m; "never", "off", and 0
/// mean no limit. `None` when it is not a count.
pub fn parse_tokens(text: &str) -> Option<Option<u64>> {
    let text = text.trim().to_lowercase().replace(['_', ','], "");
    if matches!(text.as_str(), "never" | "off" | "0") {
        return Some(None);
    }
    let (number, scale) = match text.strip_suffix('k') {
        Some(n) => (n, 1e3),
        None => match text.strip_suffix('m') {
            Some(n) => (n, 1e6),
            None => (text.as_str(), 1.0),
        },
    };
    let n: f64 = number.trim().parse().ok()?;
    (n.is_finite() && n >= 1.0).then(|| Some((n * scale).round() as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_take_suffixes_and_words_for_never() {
        assert_eq!(parse_tokens("200k"), Some(Some(200_000)));
        assert_eq!(parse_tokens(" 1.5M "), Some(Some(1_500_000)));
        assert_eq!(parse_tokens("150_000"), Some(Some(150_000)));
        assert_eq!(parse_tokens("never"), Some(None));
        assert_eq!(parse_tokens("0"), Some(None));
        for bad in ["", "k", "-5k", "lots", "0.5"] {
            assert_eq!(parse_tokens(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn saved_settings_round_trip_and_skip_what_is_unset() {
        let mut s = Settings {
            while_busy: Some(Busy::Queue),
            prompt_keys: Some(Keys::Vim),
            compact_at: Some(0),
            ..Settings::default()
        };
        s.servers.insert(
            server_key("chat", "http://localhost:11434/v1/"),
            Server {
                model: Some("qwen3:8b".into()),
                effort: None,
                windows: BTreeMap::from([("qwen3:8b".into(), 32768)]),
            },
        );
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(
            json,
            r#"{"while_busy":"queue","prompt_keys":"vim","compact_at":0,"servers":{"chat http://localhost:11434/v1":{"model":"qwen3:8b","windows":{"qwen3:8b":32768}}}}"#
        );
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), s);
        assert_eq!(
            serde_json::from_str::<Settings>("{}").unwrap(),
            Settings::default()
        );
    }
}
