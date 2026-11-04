//! Episode store (memory design, Phase B): the consolidation output layer.
//! Episodes and hypotheses live in `config/data/episodes.jsonl`, separate
//! from the transcripts journal and from brain.db. Every record carries a
//! trace back to the transcript line it was distilled from (`file` +
//! `msg_id` + 1-based `line`); a record without a valid trace is never
//! written. Appends only — no rewrite, no delete path.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const EPISODES_FILE_NAME: &str = "episodes.jsonl";
pub const CURSOR_FILE_NAME: &str = "consolidation-cursor.json";

/// The episode layer lives beside the transcripts, under the product's data
/// directory (`<root>/config/data/`).
pub fn episodes_path(root: &Path) -> PathBuf {
    root.join("config").join("data").join(EPISODES_FILE_NAME)
}

pub fn cursor_path(root: &Path) -> PathBuf {
    root.join("config").join("data").join(CURSOR_FILE_NAME)
}

/// Where a distilled record came from: the transcript file name, the
/// anchored message id, and the 1-based line in that file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EpisodeTrace {
    pub file: String,
    pub msg_id: String,
    pub line: u64,
}

/// One distilled memory record. `kind` is `episode` or `hypothesis`;
/// hypotheses always carry the lowest source tier (`AgentInference`) and
/// must never be cited as independent evidence.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EpisodeRecord {
    pub id: String,
    pub ts: String,
    pub text: String,
    pub kind: String,
    pub trace: EpisodeTrace,
    pub confidence: u8,
    pub source: String,
}

/// How far the nightly consolidation has processed: the transcript file
/// name and the 0-based line offset within it. A crash or a failed pass
/// leaves the cursor where the last fully-written window ended.
/// `last_success_at` stamps the last pass that completed without error
/// (RFC 3339) — the auto-trigger's "stale?" input.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ConsolidationCursor {
    pub file: String,
    pub line_offset: u64,
    #[serde(default)]
    pub last_success_at: Option<String>,
}

pub fn load_cursor(path: &Path) -> Result<Option<ConsolidationCursor>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cursor read failed: {}", path.display()))?;
    let cursor: ConsolidationCursor = serde_json::from_str(&raw)
        .with_context(|| format!("cursor parse failed: {}", path.display()))?;
    Ok(Some(cursor))
}

pub fn save_cursor(path: &Path, cursor: &ConsolidationCursor) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cursor dir create failed: {}", parent.display()))?;
    }
    let json = serde_json::to_string(cursor).context("cursor serialization failed")?;
    std::fs::write(path, json).with_context(|| format!("cursor write failed: {}", path.display()))
}

pub struct EpisodeStore {
    path: PathBuf,
}

impl EpisodeStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load_all(&self) -> Result<Vec<EpisodeRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let raw = std::fs::read_to_string(&self.path)
            .with_context(|| format!("episodes read failed: {}", self.path.display()))?;
        let mut records = Vec::new();
        let line_count = raw.lines().count();
        for (index, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            // A parse failure on the LAST line of a file that does not end
            // with a newline is a torn write from a kill mid-append —
            // tolerate it. Anywhere else it is real corruption and surfaces.
            let torn_tail = index + 1 == line_count && !raw.ends_with('\n');
            match serde_json::from_str::<EpisodeRecord>(line) {
                Ok(record) => records.push(record),
                Err(_) if torn_tail => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("episodes line {} parse failed", index + 1));
                }
            }
        }
        Ok(records)
    }

    /// The set of anchored msg_ids already distilled — the idempotency key.
    pub fn covered_msg_ids(&self) -> Result<HashSet<String>> {
        Ok(self
            .load_all()?
            .into_iter()
            .map(|record| record.trace.msg_id)
            .collect())
    }

    /// Append records; never rewrites or reorders existing lines.
    pub fn append(&self, records: &[EpisodeRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("episodes dir create failed: {}", parent.display()))?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("episodes open failed: {}", self.path.display()))?;
        for record in records {
            let line = serde_json::to_string(record).context("episode serialization failed")?;
            writeln!(file, "{line}").context("episode append failed")?;
        }
        file.flush().context("episode flush failed")?;
        file.sync_all().context("episode fsync failed")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(msg_id: &str, kind: &str, line: u64) -> EpisodeRecord {
        EpisodeRecord {
            id: format!("ep-{msg_id}"),
            ts: "2026-11-04T12:00:00+08:00".to_owned(),
            text: format!("distilled from {msg_id}"),
            kind: kind.to_owned(),
            trace: EpisodeTrace {
                file: "2026-11-04.jsonl".to_owned(),
                msg_id: msg_id.to_owned(),
                line,
            },
            confidence: 80,
            source: if kind == "hypothesis" {
                "AgentInference".to_owned()
            } else {
                "Consolidation".to_owned()
            },
        }
    }

    #[test]
    fn cursor_round_trips_through_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(CURSOR_FILE_NAME);
        assert!(load_cursor(&path).expect("load").is_none(), "absent cursor");
        save_cursor(
            &path,
            &ConsolidationCursor {
                file: "2026-11-04.jsonl".to_owned(),
                line_offset: 7,
                last_success_at: None,
            },
        )
        .expect("save");
        let loaded = load_cursor(&path).expect("load").expect("cursor exists");
        assert_eq!(loaded.file, "2026-11-04.jsonl");
        assert_eq!(loaded.line_offset, 7);
    }

    #[test]
    fn episodes_append_load_and_cover_msg_ids() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = EpisodeStore::new(dir.path().join(EPISODES_FILE_NAME));
        assert!(store.load_all().expect("load").is_empty());
        store
            .append(&[record("m1", "episode", 3), record("m2", "hypothesis", 5)])
            .expect("append");
        store.append(&[record("m3", "episode", 9)]).expect("append");
        let all = store.load_all().expect("load");
        assert_eq!(all.len(), 3, "append-only, three lines");
        assert_eq!(all[0].trace.msg_id, "m1");
        assert_eq!(all[1].source, "AgentInference", "hypothesis source tier");
        let covered = store.covered_msg_ids().expect("covered");
        assert_eq!(
            covered,
            HashSet::from(["m1".to_owned(), "m2".to_owned(), "m3".to_owned()])
        );
    }

    #[test]
    fn episode_paths_stay_inside_the_product_root() {
        let root = Path::new("C:\\Users\\someone\\.kangaroo");
        assert!(episodes_path(root).starts_with(root));
        assert!(cursor_path(root).starts_with(root));
        assert!(!episodes_path(root).to_string_lossy().contains(".zeroclaw"));
    }

    #[test]
    fn a_torn_tail_line_is_tolerated_but_mid_file_corruption_surfaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(EPISODES_FILE_NAME);
        let line1 = serde_json::to_string(&record("m1", "episode", 1)).expect("json");
        let line2 = serde_json::to_string(&record("m2", "episode", 2)).expect("json");
        // No trailing newline, so the last line is a candidate torn write —
        // but it parses, so it is kept.
        std::fs::write(&path, format!("{line1}\n{line2}")).expect("write");
        let store = EpisodeStore::new(path.clone());
        assert_eq!(store.load_all().expect("load").len(), 2);
        // A torn LAST line (truncated JSON, no trailing newline) is the
        // kill-mid-append signature: tolerated, earlier records intact.
        std::fs::write(&path, format!("{line1}\n{line2}\n{{\"trace\":{{\"fil"))
            .expect("torn write");
        assert_eq!(
            store.load_all().expect("load").len(),
            2,
            "torn tail tolerated"
        );
        // Corruption NOT at the tail is real corruption and must surface.
        let line3 = serde_json::to_string(&record("m3", "episode", 3)).expect("json");
        std::fs::write(&path, format!("{line1}\n{{\"trace\":{{\"fil\"\n{line3}"))
            .expect("mid corruption");
        assert!(
            store.load_all().is_err(),
            "mid-file corruption surfaces as an error"
        );
    }
}
