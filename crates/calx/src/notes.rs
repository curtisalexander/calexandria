//! Notes: a scratchpad that is yours, not the model's. It is saved to
//! `notes.md` in the calx home, kept across sessions, and never sent to the
//! model unless you move a draft from it into the prompt. It edits with vim keys.

use std::path::PathBuf;

use anyhow::Result;

use crate::editor::Editor;

pub struct Notes {
    pub editor: Editor,
    path: Option<PathBuf>,
    /// The text as last saved, to skip saves that change nothing.
    saved: String,
}

impl Notes {
    /// Loads `notes.md`; a missing file is an empty pad.
    pub fn load() -> Self {
        let path = calx_core::calx_home().ok().map(|h| h.join("notes.md"));
        let saved = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        Self {
            editor: Editor::vim(&saved),
            path,
            saved,
        }
    }

    /// Removes and returns the last paragraph, the text after the last blank
    /// line, so notes above it stay while it becomes the next prompt.
    pub fn take_draft(&mut self) -> Option<String> {
        let (keep, draft) = split_draft(&self.editor.text());
        if draft.is_empty() {
            return None;
        }
        self.editor.set(&keep);
        Some(draft)
    }

    /// Saves if anything changed since the last save.
    pub fn save(&mut self) -> Result<()> {
        let text = self.editor.text();
        if let Some(path) = &self.path
            && text != self.saved
        {
            calx_core::write_private(path, &text)?;
            self.saved = text;
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

/// Splits text into what stays and its last paragraph.
fn split_draft(text: &str) -> (String, String) {
    let trimmed = text.trim_end();
    let start = trimmed.rfind("\n\n").map_or(0, |i| i + 2);
    (
        text[..start].to_string(),
        trimmed[start..].trim().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_draft_is_only_the_last_paragraph() {
        let text = "remember: rotate the key\n\nfix the\nflaky test\n\n\n";
        assert_eq!(
            split_draft(text),
            (
                "remember: rotate the key\n\n".into(),
                "fix the\nflaky test".into()
            )
        );
        assert_eq!(split_draft("just one"), (String::new(), "just one".into()));
        assert_eq!(split_draft("\n\n").1, "");
    }
}
