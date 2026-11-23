//! Durable history: SQLite with WAL, foreign keys, and explicit migrations.
//!
//! Two tables carry the product's memory:
//! - `takes` — one recording session, its audio, and its lifecycle status.
//! - `attempts` — one transcription of a take, with the **raw** model output and
//!   the **corrected** text stored separately. Keeping them apart is what makes
//!   the dictionary auditable and what lets History propose new aliases from the
//!   user's own edits.
//!
//! Audio is written to disk *before* transcription starts, so a crash or a
//! cancelled job never loses a recording (plan §4.3, acceptance A05).

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TakeStatus {
    /// Audio is being written right now.
    Recording,
    /// Audio is on disk, waiting to be transcribed.
    Captured,
    /// Transcription succeeded.
    Transcribed,
    /// Transcription failed; the audio is still here.
    Failed,
    /// Cancelled by the user; the audio is still here.
    Cancelled,
}

impl TakeStatus {
    fn as_str(self) -> &'static str {
        match self {
            TakeStatus::Recording => "recording",
            TakeStatus::Captured => "captured",
            TakeStatus::Transcribed => "transcribed",
            TakeStatus::Failed => "failed",
            TakeStatus::Cancelled => "cancelled",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "recording" => TakeStatus::Recording,
            "captured" => TakeStatus::Captured,
            "transcribed" => TakeStatus::Transcribed,
            "failed" => TakeStatus::Failed,
            _ => TakeStatus::Cancelled,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Take {
    pub id: String,
    /// RFC 3339 UTC.
    pub created_at: String,
    pub duration_ms: i64,
    pub status: TakeStatus,
    pub audio_path: Option<PathBuf>,
    /// Whether the audio file still exists on disk.
    pub audio_present: bool,
    pub favourite: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attempt {
    pub id: String,
    pub take_id: String,
    /// Exactly what the model produced. Never rewritten.
    pub raw_text: String,
    /// Raw text after the dictionary ran. `None` if the dictionary was off.
    pub corrected_text: Option<String>,
    pub model: String,
    pub decode_ms: i64,
    /// JSON: which rules fired, for explainability.
    pub corrections_json: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub take: Take,
    pub attempt: Option<Attempt>,
}

pub struct Store {
    conn: Connection,
    root: PathBuf,
}

impl Store {
    /// Open (or create) the database under `root`, creating `root` if needed.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(root.join("audio"))?;
        let db = root.join("sotto.db");
        let conn = Connection::open(&db)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5_000)?;
        let mut s = Self { conn, root };
        s.migrate()?;
        Ok(s)
    }

    fn migrate(&mut self) -> Result<(), StoreError> {
        let v: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v >= SCHEMA_VERSION {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        tx.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS takes (
                id            TEXT PRIMARY KEY,
                created_at    TEXT NOT NULL,
                duration_ms   INTEGER NOT NULL DEFAULT 0,
                status        TEXT NOT NULL,
                audio_path    TEXT,
                favourite     INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS attempts (
                id               TEXT PRIMARY KEY,
                take_id          TEXT NOT NULL REFERENCES takes(id) ON DELETE CASCADE,
                raw_text         TEXT NOT NULL,
                corrected_text   TEXT,
                model            TEXT NOT NULL,
                decode_ms        INTEGER NOT NULL DEFAULT 0,
                corrections_json TEXT NOT NULL DEFAULT '[]',
                created_at       TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_takes_created ON takes(created_at DESC, id DESC);
            CREATE INDEX IF NOT EXISTS idx_attempts_take ON attempts(take_id, created_at DESC);
            "#,
        )?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn audio_dir(&self) -> PathBuf {
        self.root.join("audio")
    }

    /// Path a new take's audio should be written to.
    pub fn audio_path_for(&self, take_id: &str) -> PathBuf {
        self.audio_dir().join(format!("{take_id}.wav"))
    }

    pub fn create_take(&self, id: &str, created_at: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO takes (id, created_at, status) VALUES (?1, ?2, ?3)",
            params![id, created_at, TakeStatus::Recording.as_str()],
        )?;
        Ok(())
    }

    pub fn finish_take(
        &self,
        id: &str,
        duration_ms: i64,
        status: TakeStatus,
        audio: Option<&Path>,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE takes SET duration_ms=?2, status=?3, audio_path=?4 WHERE id=?1",
            params![
                id,
                duration_ms,
                status.as_str(),
                audio.map(|p| p.to_string_lossy().to_string())
            ],
        )?;
        Ok(())
    }

    pub fn add_attempt(&self, a: &Attempt) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO attempts (id, take_id, raw_text, corrected_text, model, decode_ms, corrections_json, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                a.id,
                a.take_id,
                a.raw_text,
                a.corrected_text,
                a.model,
                a.decode_ms,
                a.corrections_json,
                a.created_at
            ],
        )?;
        Ok(())
    }

    pub fn get_take(&self, id: &str) -> Result<Option<Take>, StoreError> {
        let row = self
            .conn
            .query_row(
                "SELECT id, created_at, duration_ms, status, audio_path, favourite FROM takes WHERE id=?1",
                params![id],
                row_to_take,
            )
            .optional()?;
        Ok(row.map(|mut t| {
            t.audio_present = t.audio_path.as_ref().map(|p| p.is_file()).unwrap_or(false);
            t
        }))
    }

    /// Newest first, keyset-paginated by `(created_at, id)`.
    pub fn list(&self, limit: u32, before: Option<(&str, &str)>) -> Result<Vec<HistoryEntry>, StoreError> {
        let mut out = Vec::new();
        let mut stmt;
        let rows: Vec<(String, String, i64, String, Option<String>, i64)>;
        if let Some((ca, cid)) = before {
            stmt = self.conn.prepare(
                "SELECT id, created_at, duration_ms, status, audio_path, favourite FROM takes
                 WHERE (created_at, id) < (?1, ?2)
                 ORDER BY created_at DESC, id DESC LIMIT ?3",
            )?;
            rows = stmt
                .query_map(params![ca, cid, limit], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
                })?
                .collect::<Result<_, _>>()?;
        } else {
            stmt = self.conn.prepare(
                "SELECT id, created_at, duration_ms, status, audio_path, favourite FROM takes
                 ORDER BY created_at DESC, id DESC LIMIT ?1",
            )?;
            rows = stmt
                .query_map(params![limit], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
                })?
                .collect::<Result<_, _>>()?;
        }
        for (id, created_at, duration_ms, status, audio_path, favourite) in rows {
            let take = Take {
                id: id.clone(),
                created_at,
                duration_ms,
                status: TakeStatus::parse(&status),
                audio_present: audio_path.as_ref().map(|p| Path::new(p).is_file()).unwrap_or(false),
                audio_path: audio_path.map(PathBuf::from),
                favourite: favourite != 0,
            };
            let attempt = self.latest_attempt(&id)?;
            out.push(HistoryEntry { take, attempt });
        }
        Ok(out)
    }

    pub fn latest_attempt(&self, take_id: &str) -> Result<Option<Attempt>, StoreError> {
        let row = self
            .conn
            .query_row(
                "SELECT id, take_id, raw_text, corrected_text, model, decode_ms, corrections_json, created_at
                 FROM attempts WHERE take_id=?1 ORDER BY created_at DESC LIMIT 1",
                params![take_id],
                |r| {
                    Ok(Attempt {
                        id: r.get(0)?,
                        take_id: r.get(1)?,
                        raw_text: r.get(2)?,
                        corrected_text: r.get(3)?,
                        model: r.get(4)?,
                        decode_ms: r.get(5)?,
                        corrections_json: r.get(6)?,
                        created_at: r.get(7)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Every attempt for a take, newest first.
    ///
    /// Retry appends rather than overwrites, so the History page can show what a
    /// previous attempt produced and a retry never destroys the earlier evidence
    /// (BUILD-PLAN acceptance A24).
    pub fn attempts_for(&self, take_id: &str) -> Result<Vec<Attempt>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, take_id, raw_text, corrected_text, model, decode_ms, corrections_json, created_at
             FROM attempts WHERE take_id=?1 ORDER BY created_at DESC, id DESC",
        )?;
        let rows = stmt
            .query_map(params![take_id], |r| {
                Ok(Attempt {
                    id: r.get(0)?,
                    take_id: r.get(1)?,
                    raw_text: r.get(2)?,
                    corrected_text: r.get(3)?,
                    model: r.get(4)?,
                    decode_ms: r.get(5)?,
                    corrections_json: r.get(6)?,
                    created_at: r.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The user edited a transcript. Store the corrected text without touching
    /// the raw model output.
    pub fn set_corrected(&self, attempt_id: &str, corrected: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE attempts SET corrected_text=?2 WHERE id=?1",
            params![attempt_id, corrected],
        )?;
        Ok(())
    }

    pub fn set_favourite(&self, take_id: &str, fav: bool) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE takes SET favourite=?2 WHERE id=?1",
            params![take_id, fav as i64],
        )?;
        Ok(())
    }

    /// Delete a recording, but only if it lives in this store's audio directory.
    ///
    /// The path in the database is not trusted to be ours. A take could have been
    /// pointed at a file the user keeps elsewhere (an imported recording, or a
    /// path a test supplied), and cleanup must never delete something Sotto did
    /// not write. BUILD-PLAN acceptance A26 requires exactly this: "no
    /// source/external file deletion". Both paths are canonicalised first, so a
    /// `..` in the stored path cannot escape either.
    ///
    /// Returns whether a file was actually removed.
    fn remove_managed_audio(&self, path: &Path) -> bool {
        let Ok(dir) = std::fs::canonicalize(self.audio_dir()) else {
            return false;
        };
        let Ok(target) = std::fs::canonicalize(path) else {
            return false;
        };
        if !target.starts_with(&dir) {
            return false;
        }
        std::fs::remove_file(target).is_ok()
    }

    /// Delete takes (and their attempts, by cascade). Optionally delete audio.
    pub fn delete(&self, ids: &[String], delete_audio: bool) -> Result<usize, StoreError> {
        let mut n = 0usize;
        for id in ids {
            if delete_audio {
                if let Some(t) = self.get_take(id)? {
                    if let Some(p) = t.audio_path {
                        self.remove_managed_audio(&p);
                    }
                }
            }
            n += self
                .conn
                .execute("DELETE FROM takes WHERE id=?1", params![id])?;
        }
        Ok(n)
    }

    /// Delete every recording on disk, keeping the transcripts.
    ///
    /// This is what History's "Delete all audio" button does: the recordings go,
    /// the words stay. Removing the takes themselves is a separate action, so the
    /// two can never be confused — the page promises "transcripts are kept until
    /// you delete them", and this keeps that promise.
    ///
    /// Every file in the audio directory is removed, not only the ones the
    /// database still references: a crash can leave an orphan behind, and the
    /// button says the audio is gone.
    pub fn delete_all_audio(&self) -> Result<usize, StoreError> {
        let mut removed = 0usize;
        if let Ok(rd) = std::fs::read_dir(self.audio_dir()) {
            for e in rd.flatten() {
                if e.path().is_file() && std::fs::remove_file(e.path()).is_ok() {
                    removed += 1;
                }
            }
        }
        self.conn.execute(
            "UPDATE takes SET audio_path=NULL WHERE audio_path IS NOT NULL",
            [],
        )?;
        Ok(removed)
    }

    /// Enforce retention. Returns the number of recordings removed.
    ///
    /// Favourites keep their audio: BUILD-PLAN §15.3 says a favourite protects
    /// its records from automated pruning, and the History page shows a star the
    /// user expects to mean "don't throw this away".
    pub fn enforce_retention(&self, days: Option<u32>) -> Result<usize, StoreError> {
        let Some(days) = days else { return Ok(0) };
        let cutoff = format!("-{days} days");
        let mut removed = 0usize;
        let mut stmt = self.conn.prepare(
            "SELECT id, audio_path FROM takes
             WHERE created_at < strftime('%Y-%m-%dT%H:%M:%SZ','now',?1)
               AND audio_path IS NOT NULL
               AND favourite = 0",
        )?;
        let rows: Vec<(String, Option<String>)> = stmt
            .query_map(params![cutoff], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        drop(stmt);
        for (id, path) in rows {
            if let Some(p) = path {
                self.remove_managed_audio(Path::new(&p));
            }
            self.conn.execute(
                "UPDATE takes SET audio_path=NULL WHERE id=?1",
                params![id],
            )?;
            removed += 1;
        }
        Ok(removed)
    }

    /// Trim to `max_entries`, oldest first. Audio of trimmed takes is removed.
    ///
    /// Favourites are never trimmed and do not count toward the cap: a starred
    /// take is the user saying "keep this", so silently deleting it to honour a
    /// row limit would break that promise.
    pub fn trim_to(&self, max_entries: u32) -> Result<usize, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, audio_path FROM takes
             WHERE favourite = 0
             ORDER BY created_at DESC, id DESC LIMIT -1 OFFSET ?1",
        )?;
        let rows: Vec<(String, Option<String>)> = stmt
            .query_map(params![max_entries], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        drop(stmt);
        let mut n = 0;
        for (id, path) in rows {
            if let Some(p) = path {
                self.remove_managed_audio(Path::new(&p));
            }
            n += self.conn.execute("DELETE FROM takes WHERE id=?1", params![id])?;
        }
        Ok(n)
    }

    /// Takes whose audio exists, for the storage panel.
    pub fn audio_bytes(&self) -> Result<u64, StoreError> {
        let mut total = 0u64;
        let dir = self.audio_dir();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if let Ok(m) = e.metadata() {
                    total += m.len();
                }
            }
        }
        Ok(total)
    }

    /// Recover takes left in `recording` by a crash: if the audio file exists and
    /// is non-empty, mark them captured so they can be retried.
    pub fn recover_interrupted(&self) -> Result<usize, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, audio_path FROM takes WHERE status='recording'")?;
        let rows: Vec<(String, Option<String>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        drop(stmt);
        let mut n = 0;
        for (id, path) in rows {
            let has_audio = path
                .as_ref()
                .map(|p| std::fs::metadata(p).map(|m| m.len() > 44).unwrap_or(false))
                .unwrap_or(false);
            let status = if has_audio {
                TakeStatus::Captured
            } else {
                TakeStatus::Failed
            };
            self.conn.execute(
                "UPDATE takes SET status=?2 WHERE id=?1",
                params![id, status.as_str()],
            )?;
            n += 1;
        }
        Ok(n)
    }
}

fn row_to_take(r: &rusqlite::Row) -> rusqlite::Result<Take> {
    Ok(Take {
        id: r.get(0)?,
        created_at: r.get(1)?,
        duration_ms: r.get(2)?,
        status: TakeStatus::parse(&r.get::<_, String>(3)?),
        audio_path: r.get::<_, Option<String>>(4)?.map(PathBuf::from),
        audio_present: false,
        favourite: r.get::<_, i64>(5)? != 0,
    })
}
