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
use std::collections::{HashMap, HashSet};

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

/// One proposal parsed from the summarizer reply.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LlmProposal {
    pub text: String,
    pub kind: String,
    pub anchor_msg_id: String,
    #[serde(default)]
    pub confidence: u8,
    #[serde(default)]
    pub conflicts: bool,
}

#[derive(Debug, Default)]
pub struct SelectionOutcome {
    pub winners: Vec<EpisodeRecord>,
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
    prompt.push_str("规则：\n");
    prompt.push_str("1. 每条必须引用恰好一个 msg_id 作为 anchor_msg_id，且必须来自下面的流水；\n");
    prompt.push_str("2. conflicts=true 仅当该条与既有情节矛盾或推翻既有情节；\n");
    prompt.push_str("3. hypothesis 是猜测，未确认；confidence 用 0-100；\n");
    prompt.push_str("4. 只输出 JSON 数组，元素形如 {\"text\":\"...\",\"kind\":\"episode|hypothesis\",\"anchor_msg_id\":\"...\",\"confidence\":80,\"conflicts\":false}；最多 12 条，宁缺毋滥。\n");
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
                text: "guess".to_owned(),
                kind: "hypothesis".to_owned(),
                anchor_msg_id: "m6".to_owned(),
                confidence: 50,
                conflicts: false,
            },
            proposal("orphan", "missing", false),
            proposal("covered", "m2", false),
            LlmProposal {
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
}
