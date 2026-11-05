//! Phase B consolidation logic: distill the transcript journal into the
//! episode layer. Pure selection, prompt building, and reply parsing — the
//! async orchestration (provider call, cursor writes) lives in the bin
//! adapter. Hard guarantees:
//!   * every winner anchors to a msg_id that exists in the processed window
//!     (no trace, no write);
//!   * already-covered anchors are never distilled twice (idempotency);
//!   * conflict-first slots (≤3), then recency; caps 10 episodes + 2
//!     hypotheses per pass.

use anyhow::{Context, Result};
use chrono::Local;
use std::collections::{HashMap, HashSet};
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};

use crate::infra::episodes::{EpisodeRecord, EpisodeTrace};
use crate::infra::transcript::TranscriptEntry;

pub const MAX_EPISODES_PER_PASS: usize = 10;
pub const MAX_HYPOTHESES_PER_PASS: usize = 2;
pub const MAX_CONFLICT_SLOTS: usize = 3;
/// One LLM call processes at most this many journal lines / text chars; a
/// longer day continues on the next pass (never compressed into one call).
pub const WINDOW_MAX_LINES: usize = 200;
pub const WINDOW_MAX_CHARS: usize = 24_000;

/// A journal line eligible for consolidation: user/assistant text rows only
/// (blank and tool rows are prefiltered away by the caller).
#[derive(Debug, Clone)]
pub struct CandidateLine {
    pub file: String,
    pub line: u64,
    pub msg_id: String,
    pub ts: String,
    pub role: String,
    pub text: String,
}

/// One proposal parsed from the summarizer reply. `record=false` marks a
/// candidate the summarizer judged not worth keeping (a normal decision,
/// not a failure) — the reason travels with it for the audit trail.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LlmProposal {
    #[serde(default = "default_record_true")]
    pub record: bool,
    #[serde(default)]
    pub reason: Option<String>,
    /// `record=false` vetoes carry no text (the prompt's schema omits it);
    /// the field is only written for recorded winners.
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub kind: String,
    pub anchor_msg_id: String,
    #[serde(default)]
    pub confidence: u8,
    #[serde(default)]
    pub conflicts: bool,
}

fn default_record_true() -> bool {
    true
}

#[derive(Debug, Default)]
pub struct SelectionOutcome {
    pub winners: Vec<EpisodeRecord>,
    /// Proposals the summarizer judged not worth recording (the quality
    /// veto) — a normal outcome, never counted as a failure.
    pub filtered_not_worth: usize,
    pub rejected_no_trace: usize,
    pub rejected_covered: usize,
    pub rejected_invalid_kind: usize,
    /// Two proposals in one batch claimed the same anchor — only the first
    /// (conflict-aware) may write it.
    pub rejected_duplicate_anchor: usize,
}

/// Prefilter: user/assistant rows with non-empty trimmed text.
pub fn candidate_lines(file: &str, entries: &[(u64, TranscriptEntry)]) -> Vec<CandidateLine> {
    entries
        .iter()
        .filter(|(line, entry)| {
            matches!(entry.role.as_str(), "user" | "assistant")
                && !entry.text.trim().is_empty()
                && *line > 0
        })
        .map(|(line, entry)| CandidateLine {
            file: file.to_owned(),
            line: *line,
            msg_id: entry.msg_id.clone(),
            ts: entry.ts.clone(),
            role: entry.role.clone(),
            text: entry.text.clone(),
        })
        .collect()
}

/// Parse the summarizer reply: tolerate ```json fences and surrounding
/// prose, take the first JSON array of proposals.
pub fn parse_proposals(reply: &str) -> Result<Vec<LlmProposal>> {
    let stripped = reply.trim();
    let json_body = stripped
        .strip_prefix("```json")
        .or_else(|| stripped.strip_prefix("```"))
        .unwrap_or(stripped);
    let json_body = json_body.strip_suffix("```").unwrap_or(json_body).trim();
    let start = json_body
        .find('[')
        .context("consolidation reply contains no JSON array")?;
    let end = json_body
        .rfind(']')
        .context("consolidation reply array is not closed")?;
    serde_json::from_str(&json_body[start..=end]).context("consolidation proposals parse failed")
}

/// Conflict-first (at most [`MAX_CONFLICT_SLOTS`] slots), then by anchor
/// timestamp descending; caps 10 episodes + 2 hypotheses. Proposals whose
/// anchor is unknown to the window, whose kind is neither `episode` nor
/// `hypothesis`, or whose anchor is already covered are rejected and
/// counted — never silently dropped.
pub fn select_winners(
    proposals: &[LlmProposal],
    file: &str,
    anchors: &HashMap<String, (u64, String)>,
    covered: &HashSet<String>,
) -> SelectionOutcome {
    let mut outcome = SelectionOutcome::default();
    let mut episodes: Vec<EpisodeRecord> = Vec::new();
    let mut hypotheses: Vec<EpisodeRecord> = Vec::new();
    let mut conflict_episodes: Vec<EpisodeRecord> = Vec::new();
    let mut claimed_anchors: HashSet<String> = HashSet::new();

    // Conflict proposals claim anchors FIRST regardless of input order, so
    // a regular duplicate can never crowd out the conflict signal.
    let ordered = proposals
        .iter()
        .filter(|proposal| proposal.conflicts)
        .chain(proposals.iter().filter(|proposal| !proposal.conflicts));

    for proposal in ordered {
        if !proposal.record {
            // The quality veto: the summarizer decided this candidate is not
            // worth remembering. Normal decision — audit it, never a failure.
            outcome.filtered_not_worth += 1;
            continue;
        }
        let Some((line, ts)) = anchors.get(&proposal.anchor_msg_id) else {
            outcome.rejected_no_trace += 1;
            continue;
        };
        let kind = match proposal.kind.as_str() {
            "episode" => "episode",
            "hypothesis" => "hypothesis",
            _ => {
                // Kind validation precedes anchor claiming: an invalid
                // proposal must never claim (or be blamed for) an anchor.
                outcome.rejected_invalid_kind += 1;
                continue;
            }
        };
        if covered.contains(&proposal.anchor_msg_id)
            || !claimed_anchors.insert(proposal.anchor_msg_id.clone())
        {
            outcome.rejected_covered += 1;
            continue;
        }
        let record = EpisodeRecord {
            id: format!("ep-{}", proposal.anchor_msg_id),
            ts: ts.clone(),
            text: proposal.text.trim().to_owned(),
            kind: kind.to_owned(),
            trace: EpisodeTrace {
                file: file.to_owned(),
                msg_id: proposal.anchor_msg_id.clone(),
                line: *line,
            },
            confidence: proposal.confidence,
            source: if kind == "hypothesis" {
                "AgentInference".to_owned()
            } else {
                "Consolidation".to_owned()
            },
        };
        if kind == "hypothesis" {
            hypotheses.push(record);
        } else if proposal.conflicts {
            conflict_episodes.push(record);
        } else {
            episodes.push(record);
        }
    }

    let by_ts_desc = |a: &EpisodeRecord, b: &EpisodeRecord| b.ts.cmp(&a.ts);
    conflict_episodes.sort_by(by_ts_desc);
    episodes.sort_by(by_ts_desc);
    hypotheses.sort_by(by_ts_desc);

    outcome
        .winners
        .extend(conflict_episodes.into_iter().take(MAX_CONFLICT_SLOTS));
    let conflict_taken = outcome.winners.len();
    let regular_slots = MAX_EPISODES_PER_PASS.saturating_sub(conflict_taken);
    outcome
        .winners
        .extend(episodes.into_iter().take(regular_slots));
    outcome
        .winners
        .extend(hypotheses.into_iter().take(MAX_HYPOTHESES_PER_PASS));
    outcome
}

/// Build the one-call summarization prompt: the task, the output schema,
/// the existing episodes (bounded), and the window's candidate lines.
pub fn build_prompt(candidates: &[CandidateLine], existing: &[EpisodeRecord]) -> String {
    let mut prompt = String::new();
    prompt
        .push_str("你是记忆整理器。把下面的对话流水提炼成情节（episode）与假设（hypothesis）。\n");
    prompt.push_str("\n唯一判据：日后翻出来，对理解“这个人”（用户）或“这段关系”是否有用。没有用就不要记录——哪怕一条都不产出也是正确结果。条数上限是天花板，不是目标，绝不凑数。\n");
    prompt.push_str("\n不予记录（硬否决，必须 record=false 并给一句 reason）：\n");
    prompt.push_str("- 寒暄、问候、告别；\n");
    prompt.push_str("- 会话流程（“先聊到这”“下次再说”之类）；\n");
    prompt.push_str("- 关于助手自身的内容（工具调用成败、内部状态、记忆写入是否成功）；\n");
    prompt.push_str("- 纯礼貌性表达（谢谢/好的/辛苦）。\n");
    prompt.push_str("\n规则：\n");
    prompt.push_str(
        "1. record=true 的每条必须引用恰好一个 msg_id 作为 anchor_msg_id，且必须来自下面的流水；\n",
    );
    prompt.push_str("2. conflicts=true 仅当该条与既有情节矛盾或推翻既有情节；\n");
    prompt.push_str("3. hypothesis 是猜测，未确认；confidence 用 0-100；\n");
    prompt.push_str("4. 对判断为不予记录的候选，输出 {\"record\": false, \"reason\": \"一句话理由\", \"anchor_msg_id\": \"...\"}；\n");
    prompt.push_str("5. record=true 的输出形如 {\"record\": true, \"text\": \"...\", \"kind\": \"episode|hypothesis\", \"anchor_msg_id\": \"...\", \"confidence\": 80, \"conflicts\": false}；\n");
    prompt.push_str("6. 只输出 JSON 数组，最多 12 条（含 record=false 的）。\n");
    if existing.is_empty() {
        prompt.push_str("既有情节：（空）\n");
    } else {
        prompt.push_str("既有情节：\n");
        for episode in existing.iter().take(30) {
            let text: String = episode.text.chars().take(120).collect();
            prompt.push_str(&format!("- [{}] {}\n", episode.id, text));
        }
    }
    prompt.push_str("对话流水：\n");
    for candidate in candidates {
        let text: String = candidate.text.chars().take(600).collect();
        prompt.push_str(&format!(
            "[msg_id={}] [line={}] [{}] {}\n",
            candidate.msg_id, candidate.line, candidate.role, text
        ));
    }
    prompt
}

// ── Shared consolidation service (Phase B) ──────────────────────────
//
// One implementation consumed by every trigger surface: the CLI
// `consolidate` subcommand today, the daemon's scheduler once it exists.
// The CLI adapter must stay thin — orchestration lives here.

use crate::infra::episodes::{
    ConsolidationCursor, EpisodeStore, cursor_path, episodes_path, load_cursor, save_cursor,
};
use crate::infra::transcript::{parse_entry, transcripts_dir};
use zeroclaw_api::model_provider::{ChatMessage, ChatRequest};

/// LLM call budget for one pass (the user's decision: over budget means
/// stop, cursor stays, the next pass catches up).
pub const MAX_LLM_CALLS_PER_PASS: u32 = 4;
/// Auto-trigger freshness threshold: consolidate in the background at
/// startup when the last successful pass is older than this many hours.
pub const AUTO_TRIGGER_MIN_HOURS: i64 = 20;
/// A consolidation lock older than this is stale (a crashed run) and may
/// be stolen by a new pass.
const LOCK_STALE_MINUTES: i64 = 60;

/// Best-effort exclusive lock so a manual pass and the auto-trigger (or
/// two processes) never double-append episodes. A lock left behind by a
/// crashed run is stolen after it goes stale.
pub struct ConsolidationLock {
    path: PathBuf,
    owned: bool,
}

impl ConsolidationLock {
    /// `Ok(None)` means another pass holds a fresh lock.
    pub fn acquire(dir: &Path) -> Result<Option<Self>> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("lock dir create failed: {}", dir.display()))?;
        let path = dir.join("consolidation.lock");
        if path.exists() {
            let recorded = std::fs::read_to_string(&path).unwrap_or_default();
            let stale = chrono::DateTime::parse_from_rfc3339(recorded.trim())
                .map(|then| {
                    Local::now() - then.with_timezone(&Local)
                        > chrono::Duration::minutes(LOCK_STALE_MINUTES)
                })
                .unwrap_or(true);
            if !stale {
                return Ok(None);
            }
        }
        let stamp = Local::now().to_rfc3339();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("lock create failed: {}", path.display()))?;
        writeln!(file, "{stamp}").ok();
        Ok(Some(Self { path, owned: true }))
    }
}

impl Drop for ConsolidationLock {
    fn drop(&mut self) {
        if self.owned {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The outcome of one pass; the trigger surface renders it.
#[derive(Debug, Default)]
pub struct RunSummary {
    pub already_running: bool,
    pub windows: usize,
    pub llm_calls: u32,
    pub episodes: usize,
    pub hypotheses: usize,
    pub audited: usize,
    /// Candidates the summarizer vetoed with `record=false` — a normal
    /// quality decision, never counted as a failure or a rejection.
    pub filtered_not_worth: usize,
    pub cursor: ConsolidationCursor,
    /// The promotion stage's outcome (the dream's second phase: episodes and
    /// queued inferences lifted into the profile through the narrowed upgrade
    /// channel, memory design v2 §2.3/§3).
    pub promotion: crate::infra::promotion::PromotionSummary,
}

/// Whether the journal still holds unprocessed lines after the cursor.
fn pending_lines(root: &Path, cursor: &ConsolidationCursor) -> Result<bool> {
    let transcript_dir = transcripts_dir(root);
    let Ok(entries) = std::fs::read_dir(&transcript_dir) else {
        return Ok(false);
    };
    let mut day_files: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.ends_with(".jsonl"))
        .collect();
    day_files.sort();
    let start_index = if cursor.file.is_empty() {
        0
    } else {
        match day_files.iter().position(|name| name == &cursor.file) {
            Some(position) => position,
            None => return Ok(true),
        }
    };
    for (index, name) in day_files.iter().enumerate().skip(start_index) {
        let total = std::fs::read_to_string(transcript_dir.join(name))
            .map(|raw| raw.lines().count() as u64)
            .unwrap_or(0);
        let offset = if index == start_index && name == &cursor.file {
            cursor.line_offset.min(total)
        } else {
            0
        };
        if offset < total {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The auto-trigger's gate: a stale last success AND journal lines waiting.
/// No recorded success means "never consolidated" — stale by definition.
pub fn auto_trigger_due(root: &Path, cursor: &ConsolidationCursor) -> Result<bool> {
    let stale = match &cursor.last_success_at {
        Some(stamp) => chrono::DateTime::parse_from_rfc3339(stamp)
            .map(|then| {
                Local::now() - then.with_timezone(&Local)
                    > chrono::Duration::hours(AUTO_TRIGGER_MIN_HOURS)
            })
            .unwrap_or(true),
        None => true,
    };
    Ok(stale && pending_lines(root, cursor)?)
}

/// Pick the oldest day file with unprocessed raw lines after the cursor.
fn pick_window(
    day_files: &[String],
    transcript_dir: &Path,
    cursor: &ConsolidationCursor,
) -> Result<Option<(String, Vec<String>, u64)>> {
    let start_index = if cursor.file.is_empty() {
        0
    } else {
        day_files
            .iter()
            .position(|name| name == &cursor.file)
            .with_context(|| format!("cursor file {} not in transcripts", cursor.file))?
    };
    for (index, name) in day_files.iter().enumerate().skip(start_index) {
        let raw = std::fs::read_to_string(transcript_dir.join(name))
            .with_context(|| format!("transcript read failed: {name}"))?;
        let total = raw.lines().count() as u64;
        let offset = if index == start_index && name == &cursor.file {
            cursor.line_offset.min(total)
        } else {
            0
        };
        if offset < total {
            return Ok(Some((
                name.clone(),
                raw.lines().map(str::to_owned).collect(),
                offset,
            )));
        }
    }
    Ok(None)
}

fn advance_cursor(
    cursor: &mut ConsolidationCursor,
    cursor_file: &Path,
    day_files: &[String],
    current_file: &str,
    last_processed_raw_line: Option<u64>,
    total_raw_lines: usize,
) -> Result<()> {
    let exhausted = last_processed_raw_line
        .map(|line| line >= total_raw_lines as u64)
        .unwrap_or(true);
    *cursor = if exhausted {
        let next = day_files
            .iter()
            .position(|name| name == current_file)
            .and_then(|position| day_files.get(position + 1))
            .cloned();
        match next {
            Some(next_file) => ConsolidationCursor {
                file: next_file,
                line_offset: 0,
                last_success_at: cursor.last_success_at.clone(),
            },
            None => ConsolidationCursor {
                file: current_file.to_owned(),
                line_offset: total_raw_lines as u64,
                last_success_at: cursor.last_success_at.clone(),
            },
        }
    } else {
        ConsolidationCursor {
            file: current_file.to_owned(),
            line_offset: last_processed_raw_line.expect("line present when not exhausted"),
            last_success_at: cursor.last_success_at.clone(),
        }
    };
    save_cursor(cursor_file, cursor)
}

/// One shared consolidation pass. Acquires the lock, processes up to
/// [`MAX_LLM_CALLS_PER_PASS`] windows, stamps `last_success_at` on a clean
/// exit, then runs the dream's promotion stage while still holding the lock,
/// and releases the lock. The trigger surface (CLI subcommand, daemon
/// scheduler, startup auto-trigger) renders the summary. A promotion failure
/// does not roll back the episodes stage: its cursor is already durable, and
/// the audit chain carries the failure (I-DR3).
pub async fn run_pass(
    root: &Path,
    config: &zeroclaw_config::schema::Config,
    profile_service: &crate::application::profile_service::ProfileService,
) -> Result<RunSummary> {
    let data_dir = root.join("config").join("data");
    let Some(_lock) = ConsolidationLock::acquire(&data_dir)? else {
        return Ok(RunSummary {
            already_running: true,
            ..Default::default()
        });
    };
    let provider_ref = config
        .agents
        .get("kangaroo")
        .map(|agent| agent.model_provider.to_string())
        .filter(|reference| !reference.is_empty())
        .context("consolidation needs [agents.kangaroo].model_provider")?;
    let transcript_dir = transcripts_dir(root);
    let store = EpisodeStore::new(episodes_path(root));
    let cursor_file = cursor_path(root);
    let mut cursor = load_cursor(&cursor_file)?.unwrap_or_default();

    let mut day_files: Vec<String> = std::fs::read_dir(&transcript_dir)
        .with_context(|| format!("transcripts dir read failed: {}", transcript_dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.ends_with(".jsonl"))
        .collect();
    day_files.sort();

    let mut covered = store.covered_msg_ids()?;
    let mut summary = RunSummary {
        cursor: cursor.clone(),
        ..Default::default()
    };

    while summary.llm_calls < MAX_LLM_CALLS_PER_PASS {
        let Some((file_name, raw_lines, offset)) =
            pick_window(&day_files, &transcript_dir, &cursor)?
        else {
            break;
        };
        let parsed: Vec<(u64, crate::infra::transcript::TranscriptEntry)> = raw_lines
            .iter()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .filter_map(|(index, line)| {
                // Trace lines are 1-based over the raw file, matching the
                // episodes layer contract (see episodes::EpisodeTrace).
                parse_entry(line)
                    .ok()
                    .map(|entry| (index as u64 + 1, entry))
            })
            .collect();
        let remaining: Vec<(u64, _)> = parsed
            .iter()
            .filter(|(index, _)| *index >= offset)
            .cloned()
            .collect();
        if remaining.is_empty() {
            advance_cursor(
                &mut cursor,
                &cursor_file,
                &day_files,
                &file_name,
                None,
                raw_lines.len(),
            )?;
            continue;
        }
        let (window, _end_line) = window_slice(&remaining, 0);
        let candidates = candidate_lines(&file_name, &window);
        let uncovered = candidates
            .iter()
            .filter(|candidate| !covered.contains(&candidate.msg_id))
            .count();
        if uncovered == 0 {
            // Audited skip: every anchor in this window already has an
            // episode; advance the cursor without spending an LLM call.
            summary.audited += candidates.len();
            advance_cursor(
                &mut cursor,
                &cursor_file,
                &day_files,
                &file_name,
                window.last().map(|(index, _)| *index),
                raw_lines.len(),
            )?;
            continue;
        }
        let anchors: std::collections::HashMap<String, (u64, String)> = candidates
            .iter()
            .map(|candidate| {
                (
                    candidate.msg_id.clone(),
                    (candidate.line, candidate.ts.clone()),
                )
            })
            .collect();
        let existing = store.load_all()?;
        let prompt = build_prompt(&candidates, &existing);

        let resolved =
            zeroclaw_providers::create_model_provider_from_ref_with_model(config, &provider_ref)?;
        let model = resolved
            .model
            .clone()
            .context("provider alias carries no model for consolidation")?;
        let reply = resolved
            .provider
            .chat(
                ChatRequest {
                    messages: &[ChatMessage::user(prompt)],
                    tools: None,
                    thinking: None,
                },
                &model,
                Some(0.2),
            )
            .await
            .context("consolidation LLM call failed")?;
        summary.llm_calls += 1;

        let proposals = parse_proposals(reply.text_or_empty())?;
        let outcome = select_winners(&proposals, &file_name, &anchors, &covered);
        for winner in &outcome.winners {
            covered.insert(winner.trace.msg_id.clone());
        }
        summary.episodes += outcome
            .winners
            .iter()
            .filter(|w| w.kind == "episode")
            .count();
        summary.hypotheses += outcome
            .winners
            .iter()
            .filter(|w| w.kind == "hypothesis")
            .count();
        summary.filtered_not_worth += outcome.filtered_not_worth;
        store.append(&outcome.winners)?;
        advance_cursor(
            &mut cursor,
            &cursor_file,
            &day_files,
            &file_name,
            window.last().map(|(index, _)| *index),
            raw_lines.len(),
        )?;
        summary.windows += 1;
    }

    // A clean exit (even budget-limited or zero-output) stamps success.
    cursor.last_success_at = Some(Local::now().to_rfc3339());
    save_cursor(&cursor_file, &cursor)?;
    summary.cursor = cursor.clone();

    // The dream's second phase: lift episodes and queued inferences into the
    // profile through the narrowed upgrade channel. The consolidation cursor
    // above is already saved, so a promotion failure never rolls back the
    // episodes stage's work.
    summary.promotion = crate::infra::promotion::run_promotion(root, config, profile_service)
        .await
        .context("dream promotion stage failed")?;
    Ok(summary)
}

/// The window slice of a day file: from `offset` (0-based), at most
/// WINDOW_MAX_LINES entries / WINDOW_MAX_CHARS of text.
pub fn window_slice(
    entries: &[(u64, TranscriptEntry)],
    offset: usize,
) -> (Vec<(u64, TranscriptEntry)>, usize) {
    let mut end = offset;
    let mut chars = 0usize;
    while end < entries.len() && end - offset < WINDOW_MAX_LINES {
        let cost = entries[end].1.text.chars().count();
        if end > offset && chars + cost > WINDOW_MAX_CHARS {
            break;
        }
        chars += cost;
        end += 1;
    }
    (entries[offset..end].to_vec(), end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(msg_id: &str, role: &str, text: &str) -> (u64, TranscriptEntry) {
        (
            msg_id.parse().unwrap_or(1),
            TranscriptEntry {
                ts: "2026-11-04T12:00:00+08:00".to_owned(),
                role: role.to_owned(),
                text: text.to_owned(),
                session_id: "cli-test".to_owned(),
                msg_id: msg_id.to_owned(),
                turn_no: 1,
                model: None,
            },
        )
    }

    fn proposal(text: &str, anchor: &str, conflicts: bool) -> LlmProposal {
        LlmProposal {
            record: true,
            reason: None,
            text: text.to_owned(),
            kind: "episode".to_owned(),
            anchor_msg_id: anchor.to_owned(),
            confidence: 80,
            conflicts,
        }
    }

    #[test]
    fn candidate_lines_prefilter_tools_and_blanks() {
        let entries = vec![
            (1u64, entry("m1", "user", "你好").1),
            (2u64, entry("m2", "tool", "profile_update: ok").1),
            (3u64, entry("m3", "assistant", "   ").1),
            (4u64, entry("m4", "assistant", "回复").1),
        ];
        let candidates = candidate_lines("day.jsonl", &entries);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].msg_id, "m1");
        assert_eq!(candidates[1].msg_id, "m4");
    }

    #[test]
    fn parse_proposals_tolerates_fences_and_prose() {
        let reply = "好的：\n```json\n[{\"text\":\"a\",\"kind\":\"episode\",\"anchor_msg_id\":\"m1\",\"confidence\":90,\"conflicts\":false}]\n```";
        let proposals = parse_proposals(reply).expect("parse");
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].anchor_msg_id, "m1");
        assert!(parse_proposals("no array here").is_err());
    }

    #[test]
    fn selection_is_conflict_first_then_recency_with_caps() {
        // Five regular candidates (ts desc: m5 > m4 > m3 > m2 > m1), one
        // conflict candidate on m5, one hypothesis on its own anchor m6.
        let entries: Vec<(u64, TranscriptEntry)> = (1..=6)
            .map(|i| {
                (
                    i,
                    TranscriptEntry {
                        ts: format!("2026-11-04T12:0{}:00+08:00", i),
                        role: "user".to_owned(),
                        text: format!("t{i}"),
                        session_id: "s".to_owned(),
                        msg_id: format!("m{i}"),
                        turn_no: 1,
                        model: None,
                    },
                )
            })
            .collect();
        let anchors: HashMap<String, (u64, String)> = entries
            .iter()
            .map(|(line, e)| (e.msg_id.clone(), (*line, e.ts.clone())))
            .collect();
        let proposals = vec![
            proposal("regular1", "m1", false),
            proposal("regular2", "m2", false),
            proposal("regular3", "m3", false),
            proposal("regular4", "m4", false),
            proposal("regular5", "m5", false),
            proposal("conflict", "m5", true),
            LlmProposal {
                record: true,
                reason: None,
                text: "guess".to_owned(),
                kind: "hypothesis".to_owned(),
                anchor_msg_id: "m6".to_owned(),
                confidence: 50,
                conflicts: false,
            },
            proposal("orphan", "missing", false),
            proposal("covered", "m2", false),
            LlmProposal {
                record: true,
                reason: None,
                text: "bad".to_owned(),
                kind: "nonsense".to_owned(),
                anchor_msg_id: "m1".to_owned(),
                confidence: 10,
                conflicts: false,
            },
        ];
        let covered = HashSet::from(["m2".to_owned()]);
        let outcome = select_winners(&proposals, "day.jsonl", &anchors, &covered);
        // Valid: 4 regular (m1/m3/m4; m2 covered, m5 claimed by the conflict)
        // + 1 conflict + 1 hypothesis = 5 winners. Rejected: orphan (1),
        // covered m2 twice + m5 duplicate (3), invalid kind (1).
        assert_eq!(outcome.winners.len(), 5);
        assert_eq!(outcome.rejected_no_trace, 1);
        // m2 covered twice + m5 duplicate; the invalid-kind proposal does
        // NOT reach the anchor check (kind validation runs first).
        assert_eq!(outcome.rejected_covered, 3);
        assert_eq!(outcome.rejected_invalid_kind, 1);
        // The conflict episode must be present and anchor to m5.
        assert!(
            outcome
                .winners
                .iter()
                .any(|w| w.trace.msg_id == "m5" && w.text == "conflict")
        );
        // Hypothesis keeps the lowest source tier.
        let hypothesis = outcome
            .winners
            .iter()
            .find(|w| w.kind == "hypothesis")
            .expect("hypothesis present");
        assert_eq!(hypothesis.source, "AgentInference");
        assert_eq!(hypothesis.trace.msg_id, "m6");
    }

    #[test]
    fn episode_cap_holds_at_ten_with_conflict_slots() {
        let entries: Vec<(u64, TranscriptEntry)> = (1..=20)
            .map(|i| {
                (
                    i,
                    TranscriptEntry {
                        ts: format!("2026-11-04T12:{:02}:00+08:00", i),
                        role: "user".to_owned(),
                        text: format!("t{i}"),
                        session_id: "s".to_owned(),
                        msg_id: format!("m{i}"),
                        turn_no: 1,
                        model: None,
                    },
                )
            })
            .collect();
        let anchors: HashMap<String, (u64, String)> = entries
            .iter()
            .map(|(line, e)| (e.msg_id.clone(), (*line, e.ts.clone())))
            .collect();
        let proposals: Vec<LlmProposal> = (1..=20)
            .map(|i| proposal(&format!("t{i}"), &format!("m{i}"), i <= 5))
            .collect();
        let outcome = select_winners(&proposals, "day.jsonl", &anchors, &HashSet::new());
        let episodes = outcome
            .winners
            .iter()
            .filter(|w| w.kind == "episode")
            .count();
        assert_eq!(episodes, 10, "hard cap");
        let conflicts = outcome
            .winners
            .iter()
            .filter(|w| {
                w.text.starts_with("t") && {
                    let n: usize = w.text[1..].parse().unwrap_or(0);
                    n <= 5
                }
            })
            .count();
        assert_eq!(conflicts, 3, "conflict slots capped at 3");
    }

    #[test]
    fn window_slice_respects_line_and_char_budgets() {
        let entries: Vec<(u64, TranscriptEntry)> = (1..=10)
            .map(|i| (i, entry(&format!("m{i}"), "user", &"x".repeat(100)).1))
            .collect();
        // 10 entries * 100 chars = 1000 < 24000 → all fit.
        let (window, end) = window_slice(&entries, 0);
        assert_eq!(window.len(), 10);
        assert_eq!(end, 10);
        // Char budget: 24000/100 = 240 lines... line budget 200 binds first.
        let big: Vec<(u64, TranscriptEntry)> = (1..=300)
            .map(|i| (i, entry(&format!("m{i}"), "user", "y").1))
            .collect();
        let (window, end) = window_slice(&big, 0);
        assert_eq!(window.len(), WINDOW_MAX_LINES);
        assert_eq!(end, WINDOW_MAX_LINES);
    }

    #[test]
    fn record_false_is_a_quality_veto_not_a_rejection() {
        let entries = [(1u64, entry("m1", "user", "先聊到这，下次再说").1)];
        let anchors: HashMap<String, (u64, String)> = entries
            .iter()
            .map(|(line, e)| (e.msg_id.clone(), (*line, e.ts.clone())))
            .collect();
        let proposals = vec![
            LlmProposal {
                record: false,
                reason: Some("会话流程".to_owned()),
                text: "用户表示今天先聊到这里".to_owned(),
                kind: "episode".to_owned(),
                anchor_msg_id: "m1".to_owned(),
                confidence: 60,
                conflicts: false,
            },
            proposal("真实情节", "m1", false),
        ];
        let outcome = select_winners(&proposals, "day.jsonl", &anchors, &HashSet::new());
        assert_eq!(outcome.winners.len(), 1, "only the recorded proposal wins");
        assert_eq!(outcome.filtered_not_worth, 1);
        assert_eq!(outcome.rejected_no_trace, 0);
        assert_eq!(outcome.rejected_covered, 0);
        assert_eq!(outcome.rejected_invalid_kind, 0);
    }

    #[test]
    fn consolidation_lock_is_exclusive_and_stale_locks_are_stolen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = ConsolidationLock::acquire(dir.path()).expect("acquire");
        assert!(first.is_some(), "a fresh directory yields the lock");
        let second = ConsolidationLock::acquire(dir.path()).expect("second acquire");
        assert!(second.is_none(), "a held lock blocks a second pass");
        drop(first);
        let third = ConsolidationLock::acquire(dir.path()).expect("third acquire");
        assert!(third.is_some(), "a released lock is reusable");
        // A lock left behind by a crashed run is stolen once stale.
        let stamp = (Local::now() - chrono::Duration::minutes(LOCK_STALE_MINUTES + 1)).to_rfc3339();
        std::fs::write(dir.path().join("consolidation.lock"), stamp).expect("stale stamp");
        let fourth = ConsolidationLock::acquire(dir.path()).expect("fourth acquire");
        assert!(fourth.is_some(), "a stale lock is stealable");
    }

    #[test]
    fn auto_trigger_fires_only_when_stale_and_pending() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let transcript_dir = transcripts_dir(root);
        std::fs::create_dir_all(&transcript_dir).expect("transcripts dir");
        std::fs::write(
            transcript_dir.join("2026-11-04.jsonl"),
            r#"{"ts":"2026-11-04T12:00:00+08:00","role":"user","text":"你好","session_id":"s","msg_id":"m1","turn_no":1,"model":null}"#,
        )
        .expect("transcript file");
        let fresh = ConsolidationCursor {
            file: String::new(),
            line_offset: 0,
            last_success_at: Some((Local::now() - chrono::Duration::hours(2)).to_rfc3339()),
        };
        let stale = ConsolidationCursor {
            file: String::new(),
            line_offset: 0,
            last_success_at: Some((Local::now() - chrono::Duration::hours(25)).to_rfc3339()),
        };
        assert!(
            !auto_trigger_due(root, &fresh).expect("fresh gate"),
            "a 2h-old success is fresh"
        );
        assert!(
            auto_trigger_due(root, &stale).expect("stale gate"),
            "a 25h-old success with pending lines fires"
        );
        assert!(
            auto_trigger_due(root, &ConsolidationCursor::default()).expect("never gate"),
            "no recorded success is stale by definition"
        );
        // Nothing pending → never fires, no matter how stale the stamp is.
        let empty = tempfile::tempdir().expect("empty tempdir");
        assert!(
            !auto_trigger_due(empty.path(), &stale).expect("empty gate"),
            "no journal means no trigger"
        );
    }
}
