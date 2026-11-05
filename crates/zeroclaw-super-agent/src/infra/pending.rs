//! Pending-observation queue (memory design v2, I-DR1): the holding pen for
//! the profile_update tool's inferred observations. Anything that is not a
//! user correction lands here first; the nightly dream's promotion stage
//! (infra::promotion) adjudicates the queue against episode evidence before
//! anything enters the profile. Append-only, one JSON object per line, same
//! durability contract as the episodes layer.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const PENDING_FILE_NAME: &str = "pending_observations.jsonl";

/// The queue lives beside the episodes file, under the product's data
/// directory (`<root>/config/data/`).
pub fn pending_observations_path(root: &Path) -> PathBuf {
    root.join("config").join("data").join(PENDING_FILE_NAME)
}

/// One queued inferred observation. `kind` is deliberately not stored:
/// everything enqueued here is a non-correction inference by construction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingObservation {
    pub section: String,
    pub key: String,
    pub value: String,
    pub confidence: u8,
    /// RFC 3339 enqueue timestamp.
    pub enqueued_at: String,
}

pub struct PendingQueue {
    path: PathBuf,
}

impl PendingQueue {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Append observations; never rewrites or reorders existing lines.
    pub fn append(&self, observations: &[PendingObservation]) -> Result<()> {
        if observations.is_empty() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("pending dir create failed: {}", parent.display()))?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("pending open failed: {}", self.path.display()))?;
        for observation in observations {
            let line =
                serde_json::to_string(observation).context("pending serialization failed")?;
            writeln!(file, "{line}").context("pending append failed")?;
        }
        file.flush().context("pending flush failed")?;
        file.sync_all().context("pending fsync failed")?;
        Ok(())
    }

    /// Load entries strictly after `line_offset` (the count of lines a
    /// previous dream run already consumed), each tagged with its 1-based
    /// physical line number. Blank lines keep their numbers but produce no
    /// entry. A parse failure on the last line of a file without a trailing
    /// newline is a torn write and is tolerated; anywhere else it surfaces.
    pub fn load_from(&self, line_offset: u64) -> Result<Vec<(u64, PendingObservation)>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let raw = std::fs::read_to_string(&self.path)
            .with_context(|| format!("pending read failed: {}", self.path.display()))?;
        let mut entries = Vec::new();
        let line_count = raw.lines().count() as u64;
        for (index, line) in raw.lines().enumerate() {
            let line_no = index as u64 + 1;
            if line.trim().is_empty() {
                continue;
            }
            if line_no <= line_offset {
                continue;
            }
            let torn_tail = line_no == line_count && !raw.ends_with('\n');
            match serde_json::from_str::<PendingObservation>(line) {
                Ok(observation) => entries.push((line_no, observation)),
                Err(_) if torn_tail => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("pending line {line_no} parse failed"));
                }
            }
        }
        Ok(entries)
    }

    /// Total physical line count — the unconsumed tail is `len - cursor`.
    pub fn len(&self) -> Result<u64> {
        if !self.path.exists() {
            return Ok(0);
        }
        let raw = std::fs::read_to_string(&self.path)
            .with_context(|| format!("pending read failed: {}", self.path.display()))?;
        Ok(raw.lines().count() as u64)
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(key: &str) -> PendingObservation {
        PendingObservation {
            section: "preferences".to_owned(),
            key: key.to_owned(),
            value: format!("value for {key}"),
            confidence: 70,
            enqueued_at: "2026-11-05T03:00:00+08:00".to_owned(),
        }
    }

    #[test]
    fn append_and_load_from_offsets_consumed_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = PendingQueue::new(dir.path().join(PENDING_FILE_NAME));
        assert!(queue.load_from(0).expect("load").is_empty());
        queue
            .append(&[observation("a"), observation("b")])
            .expect("append");
        queue.append(&[observation("c")]).expect("append");
        assert_eq!(queue.len().expect("len"), 3);

        let all = queue.load_from(0).expect("load all");
        assert_eq!(all.len(), 3);
        assert_eq!(
            all.iter().map(|(line, _)| *line).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "1-based physical line numbers"
        );
        // The dream's cursor: two lines consumed, only the tail remains.
        let tail = queue.load_from(2).expect("load tail");
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].0, 3);
        assert_eq!(tail[0].1.key, "c");
        assert!(queue.load_from(3).expect("exhausted").is_empty());
    }

    #[test]
    fn a_torn_tail_line_is_tolerated_but_mid_file_corruption_surfaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(PENDING_FILE_NAME);
        let line1 = serde_json::to_string(&observation("a")).expect("json");
        let line2 = serde_json::to_string(&observation("b")).expect("json");
        // Torn LAST line (truncated JSON, no trailing newline): tolerated.
        std::fs::write(&path, format!("{line1}\n{line2}\n{{\"section\":\"pref"))
            .expect("torn write");
        let queue = PendingQueue::new(path.clone());
        assert_eq!(queue.load_from(0).expect("load").len(), 2);
        // Corruption NOT at the tail is real corruption and must surface.
        std::fs::write(&path, format!("{line1}\n{{\"section\":\"pref\"\n{line2}"))
            .expect("mid corruption");
        assert!(
            queue.load_from(0).is_err(),
            "mid-file corruption surfaces as an error"
        );
        // The torn-tail line never becomes visible even for a fresh cursor.
        std::fs::write(&path, format!("{line1}\n{{\"section\":\"pref")).expect("torn again");
        let queue = PendingQueue::new(path);
        let entries = queue.load_from(0).expect("load");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, 1);
    }

    #[test]
    fn pending_path_stays_inside_the_product_root() {
        let root = Path::new("C:\\Users\\someone\\.kangaroo");
        assert!(pending_observations_path(root).starts_with(root));
        assert!(
            !pending_observations_path(root)
                .to_string_lossy()
                .contains(".zeroclaw")
        );
    }
}
