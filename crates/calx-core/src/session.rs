//! Sessions: every conversation, saved in SQLite (`sessions.db` in the calx
//! home directory) so it can be resumed, searched, and later compacted.
//!
//! Each session is an append-only list of items, numbered from 1 in the order
//! they happened. The numbers are stable, so later features such as compaction
//! can refer to items by number without rewriting them.
//!
//! The database holds whole conversations, including anything a tool read, so
//! it is as private as the files it came from. The calx home directory is
//! readable only by the user (on Unix).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

use crate::message::Item;

const SCHEMA_VERSION: i64 = 1;
const SCHEMA: &str = "
BEGIN;
CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL,  -- Unix milliseconds
    updated_at INTEGER NOT NULL,
    cwd TEXT NOT NULL,
    model TEXT NOT NULL,
    title TEXT NOT NULL           -- the start of the first prompt
);
CREATE TABLE items (
    session_id TEXT NOT NULL REFERENCES sessions(id),
    seq INTEGER NOT NULL,         -- 1, 2, 3, ... within the session
    created_at INTEGER NOT NULL,
    item TEXT NOT NULL,           -- the Item as JSON
    PRIMARY KEY (session_id, seq)
);
CREATE INDEX sessions_by_cwd ON sessions(cwd, updated_at);
PRAGMA user_version = 1;
COMMIT;
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub cwd: String,
    pub model: String,
    pub title: String,
    /// Unix milliseconds.
    pub updated_at: u64,
    pub item_count: u64,
}

pub struct Sessions {
    conn: Connection,
}

impl Sessions {
    pub fn open() -> Result<Self> {
        let home = crate::calx_home()?;
        crate::create_private_dir(&home)?;
        Self::open_at(&home.join("sessions.db"))
    }

    pub fn open_at(path: &Path) -> Result<Self> {
        let conn =
            Connection::open(path).with_context(|| format!("could not open {}", path.display()))?;
        // Several calx processes may share the database; WAL lets readers and
        // one writer work at once, and the timeout makes writers wait their turn.
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        match version {
            0 => conn
                .execute_batch(SCHEMA)
                .context("could not create the sessions database")?,
            SCHEMA_VERSION => {}
            v => bail!(
                "{} is version {v}, newer than this calx understands",
                path.display()
            ),
        }
        Ok(Self { conn })
    }

    /// Appends items to a session, creating the session if it is new. The
    /// items are saved together or not at all.
    pub fn append(&self, id: &str, cwd: &str, model: &str, items: &[Item]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let title = items
            .iter()
            .find_map(|item| match item {
                Item::User { text } => Some(title_of(text)),
                _ => None,
            })
            .unwrap_or_default();
        let tx = self.conn.unchecked_transaction()?;
        // Each save gets a later time than any before it, so "most recent"
        // is exact even for saves within the same millisecond.
        let last: i64 = tx.query_row(
            "SELECT COALESCE(MAX(updated_at), 0) FROM sessions",
            [],
            |r| r.get(0),
        )?;
        let now = (now_ms() as i64).max(last + 1);
        tx.execute(
            "INSERT INTO sessions (id, created_at, updated_at, cwd, model, title)
             VALUES (?1, ?2, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET updated_at = ?2, model = ?4",
            params![id, now, cwd, model, title],
        )?;
        let next: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM items WHERE session_id = ?1",
            [id],
            |r| r.get(0),
        )?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO items (session_id, seq, created_at, item) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (i, item) in items.iter().enumerate() {
                insert.execute(params![
                    id,
                    next + i as i64,
                    now,
                    serde_json::to_string(item)?
                ])?;
            }
        }
        tx.commit().context("could not save the session")
    }

    /// A session's items, in order.
    pub fn load(&self, id: &str) -> Result<Vec<Item>> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, item FROM items WHERE session_id = ?1 ORDER BY seq")?;
        let rows = stmt.query_map([id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        rows.map(|row| {
            let (seq, json) = row?;
            serde_json::from_str(&json)
                .with_context(|| format!("item {seq} of session {id} is not valid"))
        })
        .collect()
    }

    /// The full ID of the one session whose ID starts with `prefix`.
    pub fn resolve(&self, prefix: &str) -> Result<String> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM sessions WHERE substr(id, 1, length(?1)) = ?1 LIMIT 2")?;
        let mut ids: Vec<String> = stmt
            .query_map([prefix], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        match ids.len() {
            0 => bail!("no session ID starts with {prefix:?}; `calx sessions` lists them"),
            1 => Ok(ids.remove(0)),
            _ => bail!("more than one session ID starts with {prefix:?}; use more of the ID"),
        }
    }

    /// The most recently updated session started in `cwd`.
    pub fn latest(&self, cwd: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM sessions WHERE cwd = ?1 ORDER BY updated_at DESC LIMIT 1",
                [cwd],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The most recently updated sessions, newest first.
    pub fn list(&self, limit: usize) -> Result<Vec<SessionSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, cwd, model, title, updated_at,
                    (SELECT COUNT(*) FROM items WHERE session_id = sessions.id)
             FROM sessions ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok(SessionSummary {
                id: r.get(0)?,
                cwd: r.get(1)?,
                model: r.get(2)?,
                title: r.get(3)?,
                updated_at: r.get::<_, i64>(4)? as u64,
                item_count: r.get::<_, i64>(5)? as u64,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

/// The first line of a prompt, cut to 80 characters.
fn title_of(text: &str) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default()
        .trim();
    let mut title: String = line.chars().take(80).collect();
    if line.chars().count() > 80 {
        title.push('…');
    }
    title
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolCall;

    fn temp_db(name: &str) -> Sessions {
        let dir = std::env::temp_dir().join(format!("calx-sessions-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Sessions::open_at(&dir.join("sessions.db")).unwrap()
    }

    #[test]
    fn appends_continue_the_sequence_and_load_in_order() {
        let db = temp_db("append");
        let turn1 = vec![
            Item::User {
                text: "\n  fix the build\nsecond line".into(),
            },
            Item::ToolCall {
                call: ToolCall {
                    id: "c1".into(),
                    name: "shell".into(),
                    arguments: "{}".into(),
                },
                raw: Some(serde_json::json!({ "id": "fc_1", "encrypted": "x" })),
            },
            Item::ToolResult {
                call_id: "c1".into(),
                output: "ok".into(),
            },
        ];
        let turn2 = vec![
            Item::User {
                text: "again".into(),
            },
            Item::Assistant {
                text: "done".into(),
                raw: None,
            },
        ];
        db.append("s1", "/w", "m1", &turn1).unwrap();
        db.append(
            "s2",
            "/w",
            "m1",
            &[Item::User {
                text: "other".into(),
            }],
        )
        .unwrap();
        db.append("s1", "/w", "m2", &turn2).unwrap();

        let all: Vec<Item> = turn1.into_iter().chain(turn2).collect();
        assert_eq!(db.load("s1").unwrap(), all);
        let seqs: Vec<i64> = db
            .conn
            .prepare("SELECT seq FROM items WHERE session_id = 's1' ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5]);

        // The title comes from the first prompt and does not change; the model does.
        let s1 = db
            .list(10)
            .unwrap()
            .into_iter()
            .find(|s| s.id == "s1")
            .unwrap();
        assert_eq!(
            (s1.title.as_str(), s1.model.as_str(), s1.item_count),
            ("fix the build", "m2", 5)
        );
        // s1 was updated last, so it is the latest even though s2 was created later.
        assert_eq!(db.latest("/w").unwrap().as_deref(), Some("s1"));
        assert_eq!(db.latest("/elsewhere").unwrap(), None);
    }

    #[test]
    fn resolves_unique_id_prefixes_only() {
        let db = temp_db("resolve");
        let user = [Item::User { text: "hi".into() }];
        db.append("abc123", "/w", "m", &user).unwrap();
        db.append("abd456", "/w", "m", &user).unwrap();
        assert_eq!(db.resolve("abc").unwrap(), "abc123");
        assert!(
            db.resolve("ab")
                .unwrap_err()
                .to_string()
                .contains("more than one")
        );
        assert!(
            db.resolve("%")
                .unwrap_err()
                .to_string()
                .contains("no session")
        );
    }
}
