//! Notes: a scratchpad that is yours, not the model's. It is saved to
//! `notes.md` in the calx home, kept across sessions, and never sent to the
//! model unless you move a draft from it into the prompt.

use std::path::PathBuf;

use anyhow::Result;

pub struct Notes {
    pub text: String,
    path: Option<PathBuf>,
    /// Edited since the last save.
    dirty: bool,
}

impl Notes {
    /// Loads `notes.md`; a missing file is an empty pad.
    pub fn load() -> Self {
        let path = calx_core::calx_home().ok().map(|h| h.join("notes.md"));
        let text = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        Self {
            text,
            path,
            dirty: false,
        }
    }

    pub fn push_str(&mut self, text: &str) {
        self.text.push_str(&text.replace("\r\n", "\n"));
        self.dirty = true;
    }

    pub fn pop(&mut self) {
        self.dirty |= self.text.pop().is_some();
    }

    /// Removes and returns the last paragraph, the text after the last blank
    /// line, so notes above it stay while it becomes the next prompt.
    pub fn take_draft(&mut self) -> Option<String> {
        let trimmed = self.text.trim_end();
        let start = trimmed.rfind("\n\n").map_or(0, |i| i + 2);
        let draft = trimmed[start..].trim().to_string();
        if draft.is_empty() {
            return None;
        }
        self.text.truncate(start);
        self.dirty = true;
        Some(draft)
    }

    /// Saves if anything changed since the last save.
    pub fn save(&mut self) -> Result<()> {
        if let (true, Some(path)) = (self.dirty, &self.path) {
            calx_core::write_private(path, &self.text)?;
            self.dirty = false;
        }
        Ok(())
    }

    pub fn path(&self) -> String {
        self.path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad(text: &str) -> Notes {
        Notes {
            text: text.into(),
            path: None,
            dirty: false,
        }
    }

    #[test]
    fn take_draft_moves_only_the_last_paragraph() {
        let mut notes = pad("remember: rotate the key\n\nfix the\nflaky test\n\n\n");
        assert_eq!(notes.take_draft().as_deref(), Some("fix the\nflaky test"));
        assert_eq!(notes.text, "remember: rotate the key\n\n");
        assert_eq!(
            notes.take_draft().as_deref(),
            Some("remember: rotate the key")
        );
        assert_eq!(notes.text, "");
        assert_eq!(notes.take_draft(), None);
    }
}
