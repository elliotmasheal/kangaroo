//! Dream-run audit trail (memory design v2 §3.4): every promotion pass
//! appends its event chain to `dream_run.jsonl` and keeps its resume state in
//! `state.json`. The chain answers four questions without any other source:
//! what was read, what was concluded, what was written, and why. A
//! `RunStarted` without a matching `RunFinished` is a failed run (I-DR3).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The audit chain and state live in their own subdirectory of the product's
/// data directory: `<root>/config/data/dream/`.
pub fn dream_dir(root: &Path) -> PathBuf {
    root.join("config").join("data").join("dream")
}

pub fn events_path(root: &Path) -> PathBuf {
    dream_dir(root).join("dream_run.jsonl")
}

pub fn state_path(root: &Path) -> PathBuf {
    dream_dir(root).join("state.json")
}

/// One event in a dream run's audit chain. Serde-tagged so each JSONL line is
/// self-describing; field lists follow the design doc's event structure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum DreamEvent {
    RunStarted {
        run_id: String,
        trigger: String,
        started_at: String,
    },
    Candidate {
        run_id: String,
        section: String,
        key: String,
        value: String,
        confidence: u8,
        episode_ids: Vec<String>,
        pending_refs: Vec<u64>,
    },
    /// A legal candidate the aggregate's precedence adjudication kept out
    /// (e.g. it tried to override a user's explicit fact).
    Superseded {
        run_id: String,
        section: String,
        key: String,
        reason: String,
    },
    /// The narrowed upgrade channel refused the candidate (I-M7): broken or
    /// self-referential evidence, a corrections-section write, and so on.
    UpgradeRefused {
        run_id: String,
        section: String,
        key: String,
        reason: String,
    },
    RetireProposed {
        run_id: String,
        section: String,
        key: String,
        reason: String,
        episode_ids: Vec<String>,
    },
    /// One committed batch. The profile's own revision log stays the single
    /// source of truth for revision ids, so this event carries fact keys.
    Committed {
        run_id: String,
        fact_keys: Vec<String>,
    },
    Skipped {
        run_id: String,
        reason: String,
    },
    RunFinished {
        run_id: String,
        finished_at: String,
        candidates: usize,
        written: usize,
        retired: usize,
        refused: usize,
        superseded: usize,
    },
    /// The pass failed mid-run; the episodes stage's cursor is NOT rolled
    /// back (its writes are already durable and idempotent).
    RunFailed {
        run_id: String,
        finished_at: String,
        error: String,
    },
}

/// The dream's resume state. `pending_cursor` counts the physical lines of
/// the pending-observation queue already submitted to a successful promotion
/// call; episode novelty is derived from `last_success_at` versus episode
/// timestamps, so no second cursor is kept.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DreamState {
    pub last_run_id: Option<String>,
    pub last_success_at: Option<String>,
    #[serde(default)]
    pub pending_cursor: u64,
}

pub fn append_event(path: &Path, event: &DreamEvent) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("dream dir create failed: {}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("dream event log open failed: {}", path.display()))?;
    let line = serde_json::to_string(event).context("dream event serialization failed")?;
    writeln!(file, "{line}").context("dream event append failed")?;
    file.flush().context("dream event flush failed")?;
    file.sync_all().context("dream event fsync failed")?;
    Ok(())
}

pub fn load_state(path: &Path) -> Result<Option<DreamState>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("dream state read failed: {}", path.display()))?;
    let state: DreamState = serde_json::from_str(&raw)
        .with_context(|| format!("dream state parse failed: {}", path.display()))?;
    Ok(Some(state))
}

pub fn save_state(path: &Path, state: &DreamState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("dream dir create failed: {}", parent.display()))?;
    }
    let json = serde_json::to_string(state).context("dream state serialization failed")?;
    std::fs::write(path, json)
        .with_context(|| format!("dream state write failed: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_event_chain_appends_and_parses_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dream_run.jsonl");
        let events = vec![
            DreamEvent::RunStarted {
                run_id: "dream-1".to_owned(),
                trigger: "consolidation".to_owned(),
                started_at: "2026-11-05T03:00:00+08:00".to_owned(),
            },
            DreamEvent::Candidate {
                run_id: "dream-1".to_owned(),
                section: "preferences".to_owned(),
                key: "editor".to_owned(),
                value: "Neovim".to_owned(),
                confidence: 80,
                episode_ids: vec!["ep-m1".to_owned()],
                pending_refs: vec![3],
            },
            DreamEvent::UpgradeRefused {
                run_id: "dream-1".to_owned(),
                section: "corrections".to_owned(),
                key: "tone".to_owned(),
                reason: "corrections section is user-only".to_owned(),
            },
            DreamEvent::RetireProposed {
                run_id: "dream-1".to_owned(),
                section: "goals".to_owned(),
                key: "trip".to_owned(),
                reason: "contradicted".to_owned(),
                episode_ids: vec!["ep-m2".to_owned()],
            },
            DreamEvent::Committed {
                run_id: "dream-1".to_owned(),
                fact_keys: vec!["preferences.editor".to_owned()],
            },
            DreamEvent::RunFinished {
                run_id: "dream-1".to_owned(),
                finished_at: "2026-11-05T03:00:05+08:00".to_owned(),
                candidates: 2,
                written: 1,
                retired: 1,
                refused: 1,
                superseded: 0,
            },
        ];
        for event in &events {
            append_event(&path, event).expect("append");
        }
        let raw = std::fs::read_to_string(&path).expect("read");
        let parsed: Vec<DreamEvent> = raw
            .lines()
            .map(|line| serde_json::from_str(line).expect("event json"))
            .collect();
        assert_eq!(parsed, events, "the chain round-trips verbatim");
        // Each line is self-describing through the tagged variant name.
        assert!(raw.contains("\"event\":\"run_started\""));
        assert!(raw.contains("\"event\":\"upgrade_refused\""));
        assert!(raw.contains("\"event\":\"retire_proposed\""));
    }

    #[test]
    fn state_round_trips_and_absent_means_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        assert!(load_state(&path).expect("load").is_none());
        let state = DreamState {
            last_run_id: Some("dream-1".to_owned()),
            last_success_at: Some("2026-11-05T03:00:05+08:00".to_owned()),
            pending_cursor: 7,
        };
        save_state(&path, &state).expect("save");
        assert_eq!(load_state(&path).expect("load"), Some(state));
    }

    #[test]
    fn dream_paths_stay_inside_the_product_root() {
        let root = Path::new("C:\\Users\\someone\\.kangaroo");
        assert!(events_path(root).starts_with(root));
        assert!(state_path(root).starts_with(root));
        assert!(!events_path(root).to_string_lossy().contains(".zeroclaw"));
    }
}
