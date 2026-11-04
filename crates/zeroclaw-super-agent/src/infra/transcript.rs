//! Append-only conversation transcripts — the never-forgotten journal layer
//! (memory design, Phase A). Every user turn, assistant reply, and tool
//! execution is appended verbatim to `config/data/transcripts/YYYY-MM-DD.jsonl`
//! under the product data root. The layer is a separate source of truth: it
//! never touches brain.db, offers no delete or rewrite path, and a failed
//! append degrades to a warning plus a drop counter — the conversation never
//! blocks or dies on transcript IO.

use anyhow::Context;
use chrono::Local;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use zeroclaw_api::observability_traits::{Observer, ObserverEvent, ObserverMetric};
use zeroclaw_log::{Action, Event, EventOutcome, record};

/// The transcripts directory under the product data root:
/// `<root>/config/data/transcripts/` — always inside the product's own tree
/// (the startup isolation assertion covers the root; see `infra::paths`).
pub fn transcripts_dir(root: &Path) -> PathBuf {
    root.join("config").join("data").join("transcripts")
}

/// Bound a tool summary: names and a short prefix only, never full
/// arguments (avoids double-journaling sensitive payloads).
fn bounded_summary(text: &str) -> String {
    const MAX: usize = 160;
    if text.chars().count() <= MAX {
        text.to_owned()
    } else {
        let cut: String = text.chars().take(MAX).collect();
        format!("{cut}…")
    }
}

#[derive(Serialize)]
struct TranscriptEntry<'a> {
    ts: &'a str,
    role: &'a str,
    text: &'a str,
    session_id: &'a str,
    msg_id: String,
    turn_no: u64,
    model: Option<&'a str>,
}

/// Returns `(local day "YYYY-MM-DD", RFC 3339 timestamp)`.
type NowFn = Box<dyn Fn() -> (String, String) + Send + Sync>;

struct State {
    day: String,
    file: Option<File>,
    seq: u64,
    turn_no: u64,
}

pub struct TranscriptWriter {
    dir: PathBuf,
    session_id: String,
    model: Option<String>,
    now: NowFn,
    state: Mutex<State>,
    dropped: AtomicU64,
}

impl TranscriptWriter {
    /// Production constructor: local-day files and local RFC 3339 stamps.
    pub fn new(dir: PathBuf, session_id: String, model: Option<String>) -> Self {
        Self::with_clock(
            dir,
            session_id,
            model,
            Box::new(|| {
                (
                    Local::now().format("%Y-%m-%d").to_string(),
                    Local::now().to_rfc3339(),
                )
            }),
        )
    }

    /// Test constructor with an injectable clock.
    pub fn with_clock(dir: PathBuf, session_id: String, model: Option<String>, now: NowFn) -> Self {
        Self {
            dir,
            session_id,
            model,
            now,
            state: Mutex::new(State {
                day: String::new(),
                file: None,
                seq: 0,
                turn_no: 0,
            }),
            dropped: AtomicU64::new(0),
        }
    }

    /// How many records failed to append since construction. A nonzero count
    /// means the journal has holes (each one warned at WARN level); the
    /// conversation itself never blocked.
    pub fn dropped_records(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn record_user(&self, text: &str) {
        let mut turn_no = None;
        if let Ok(mut state) = self.state.lock() {
            state.turn_no += 1;
            turn_no = Some(state.turn_no);
        }
        self.record("user", text, turn_no);
    }

    pub fn record_assistant(&self, text: &str) {
        self.record("assistant", text, None);
    }

    pub fn record_tool(&self, tool: &str, success: bool, summary: &str) {
        let text = format!(
            "{tool}: {} ({})",
            if success { "ok" } else { "failed" },
            bounded_summary(summary)
        );
        self.record("tool", &text, None);
    }

    fn record(&self, role: &str, text: &str, turn_no: Option<u64>) {
        let (day, ts) = (self.now)();
        let result = self
            .state
            .lock()
            .map_err(|_| anyhow::Error::msg("transcript state mutex poisoned"))
            .and_then(|mut state| {
                state.seq += 1;
                let entry = TranscriptEntry {
                    ts: &ts,
                    role,
                    text,
                    session_id: &self.session_id,
                    msg_id: format!("{}-{:06}", self.session_id, state.seq),
                    turn_no: turn_no.unwrap_or(state.turn_no),
                    model: self.model.as_deref(),
                };
                let line = serde_json::to_string(&entry)
                    .context("transcript entry serialization failed")?;
                if state.day != day || state.file.is_none() {
                    std::fs::create_dir_all(&self.dir)
                        .with_context(|| format!("transcript dir {}", self.dir.display()))?;
                    let file = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(self.dir.join(format!("{day}.jsonl")))
                        .with_context(|| format!("transcript file for {day}"))?;
                    state.day = day;
                    state.file = Some(file);
                }
                let file = state
                    .file
                    .as_mut()
                    .expect("transcript file is open after the day check");
                writeln!(file, "{line}").context("transcript append failed")?;
                file.flush().context("transcript flush failed")?;
                file.sync_all().context("transcript fsync failed")?;
                Ok(())
            });
        if let Err(error) = result {
            let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            record!(
                WARN,
                Event::new(module_path!(), Action::Note)
                    .with_outcome(EventOutcome::Failure)
                    .with_attrs(serde_json::json!({
                        "dropped_total": dropped,
                        "error": format!("{error:#}"),
                    })),
                "transcript.append degraded; conversation continues"
            );
        }
    }
}

/// Observer adapter: journals tool executions (name, outcome, bounded
/// summary) without disturbing the runtime's event flow. Everything else is
/// delegated to nothing — the transcript is a journal, not a metrics sink.
pub struct TranscriptObserver {
    writer: Arc<TranscriptWriter>,
}

impl TranscriptObserver {
    pub fn new(writer: Arc<TranscriptWriter>) -> Self {
        Self { writer }
    }
}

impl Observer for TranscriptObserver {
    fn record_event(&self, event: &ObserverEvent) {
        if let ObserverEvent::ToolCall {
            tool,
            success,
            arguments,
            ..
        } = event
        {
            let summary = arguments
                .as_deref()
                .map(bounded_summary)
                .unwrap_or_default();
            self.writer.record_tool(tool, *success, &summary);
        }
    }

    fn record_metric(&self, _metric: &ObserverMetric) {}

    fn name(&self) -> &str {
        "transcript"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_clock(day: &'static str) -> NowFn {
        Box::new(move || (day.to_owned(), "2026-11-04T12:00:00+08:00".to_owned()))
    }

    #[test]
    fn records_append_jsonl_lines_with_expected_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = TranscriptWriter::with_clock(
            dir.path().to_path_buf(),
            "cli-test".to_owned(),
            Some("deepseek.deepseek".to_owned()),
            fixed_clock("2026-11-04"),
        );
        writer.record_user("你好");
        writer.record_assistant("你好，有什么事直接说就行。");
        assert_eq!(writer.dropped_records(), 0);
        let content =
            std::fs::read_to_string(dir.path().join("2026-11-04.jsonl")).expect("transcript");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "two appended lines: {content}");
        let first: serde_json::Value = serde_json::from_str(lines[0]).expect("json");
        assert_eq!(first["role"], "user");
        assert_eq!(first["text"], "你好");
        assert_eq!(first["session_id"], "cli-test");
        assert_eq!(first["turn_no"], 1);
        assert_eq!(first["model"], "deepseek.deepseek");
        assert!(first["msg_id"].as_str().unwrap().starts_with("cli-test-"));
        let second: serde_json::Value = serde_json::from_str(lines[1]).expect("json");
        assert_eq!(second["role"], "assistant");
        assert_eq!(second["turn_no"], 1, "assistant keeps the user turn number");
    }

    #[test]
    fn a_new_day_starts_a_new_file_and_both_stay_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let day_one = fixed_clock("2026-11-04");
        let writer = TranscriptWriter::with_clock(
            dir.path().to_path_buf(),
            "cli-test".to_owned(),
            None,
            day_one,
        );
        writer.record_user("first");
        drop(writer);
        let writer = TranscriptWriter::with_clock(
            dir.path().to_path_buf(),
            "cli-test-2".to_owned(),
            None,
            fixed_clock("2026-11-05"),
        );
        writer.record_user("second");
        let day_one_lines = std::fs::read_to_string(dir.path().join("2026-11-04.jsonl"))
            .expect("day one file")
            .lines()
            .count();
        let day_two_lines = std::fs::read_to_string(dir.path().join("2026-11-05.jsonl"))
            .expect("day two file")
            .lines()
            .count();
        assert_eq!(day_one_lines, 1);
        assert_eq!(day_two_lines, 1);
    }

    #[test]
    fn appending_never_rewrites_earlier_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("2026-11-04.jsonl");
        {
            let writer = TranscriptWriter::with_clock(
                dir.path().to_path_buf(),
                "cli-test".to_owned(),
                None,
                fixed_clock("2026-11-04"),
            );
            writer.record_user("one");
        }
        let after_first = std::fs::read_to_string(&path).expect("file");
        let writer = TranscriptWriter::with_clock(
            dir.path().to_path_buf(),
            "cli-test-2".to_owned(),
            None,
            fixed_clock("2026-11-04"),
        );
        writer.record_user("two");
        let after_second = std::fs::read_to_string(&path).expect("file");
        assert!(after_second.starts_with(&after_first), "append only");
        assert_eq!(after_second.lines().count(), 2);
    }

    #[test]
    fn write_failure_degrades_to_a_counter_and_never_panics() {
        // A regular FILE sits where the transcripts DIRECTORY must be created,
        // so every open fails — the model of an unwritable/locked location.
        let dir = tempfile::tempdir().expect("tempdir");
        let writer_dir = dir.path().join("transcripts");
        std::fs::write(&writer_dir, b"not a directory trap").expect("blocker file");
        let writer = TranscriptWriter::with_clock(
            writer_dir,
            "cli-test".to_owned(),
            None,
            fixed_clock("2026-11-04"),
        );
        writer.record_user("one");
        writer.record_assistant("two");
        assert_eq!(
            writer.dropped_records(),
            2,
            "both records counted as dropped"
        );
    }

    #[test]
    fn tool_records_carry_name_outcome_and_bounded_summary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = TranscriptWriter::with_clock(
            dir.path().to_path_buf(),
            "cli-test".to_owned(),
            None,
            fixed_clock("2026-11-04"),
        );
        let long = "x".repeat(500);
        writer.record_tool("profile_update", true, &long);
        let content =
            std::fs::read_to_string(dir.path().join("2026-11-04.jsonl")).expect("transcript");
        let entry: serde_json::Value =
            serde_json::from_str(content.lines().next().unwrap()).expect("json");
        assert_eq!(entry["role"], "tool");
        let text = entry["text"].as_str().unwrap();
        assert!(text.starts_with("profile_update: ok ("));
        assert!(text.chars().count() < 200, "summary stays bounded");
    }

    #[test]
    fn transcripts_dir_stays_inside_the_product_root() {
        let root = Path::new("C:\\Users\\someone\\.kangaroo");
        let dir = transcripts_dir(root);
        assert!(dir.starts_with(root));
        assert!(!dir.to_string_lossy().contains(".zeroclaw"));
    }
}
