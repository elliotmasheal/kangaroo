//! Dream promotion stage (memory design v2 §2.3/§3, invariants I-M7 and
//! I-DR1): the ONLY channel that lifts dream material into the user profile.
//! Layout mirrors `consolidation.rs`: pure planning and prompt building are
//! directly testable, the IO orchestration is one thin `run_promotion`.
//!
//! The narrowed upgrade channel, enforced by `plan_promotion`:
//!   * a candidate must cite at least one existing `kind == "episode"`
//!     record — empty evidence, unknown ids, and hypothesis-only evidence are
//!     refused (`UpgradeRefused`), because the agent's own inference may never
//!     vouch for itself;
//!   * the corrections section is never written by the dream (I-DR1);
//!   * retirement is accepted only for facts currently sourced from
//!     `AgentInference` — user-sourced facts are untouchable.

use anyhow::{Context, Result};
use chrono::Local;
use std::collections::HashMap;
use std::path::Path;

use crate::application::profile_service::{MAX_PROFILE_UPDATES_PER_TURN, ProfileService};
use crate::domain::profile::{
    ChangeSource, Observation, ObservationKind, RecordDecision, SectionKind, UserProfile,
};
use crate::infra::dream_audit::{self, DreamEvent, DreamState};
use crate::infra::episodes::{EpisodeRecord, EpisodeStore, episodes_path};
use crate::infra::pending::{PendingObservation, PendingQueue, pending_observations_path};
use zeroclaw_api::model_provider::{ChatMessage, ChatRequest};

/// LLM call budget for one promotion pass. Placeholder calibration: one call
/// reads the bounded episode/pending windows below; a larger budget is a
/// registered pending decision (see .workbuddy/design/pending-decisions.md).
pub const MAX_PROMOTION_CALLS_PER_PASS: u32 = 1;
/// Prompt windows: the most recent episodes and the oldest unconsumed pending
/// observations offered to one call.
pub const PROMPT_EPISODE_LIMIT: usize = 50;
pub const PROMPT_PENDING_LIMIT: usize = 30;

/// One promotion candidate parsed from the dream reply.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PromotionProposal {
    pub section: String,
    pub key: String,
    pub value: String,
    pub kind: String,
    #[serde(default)]
    pub confidence: u8,
    #[serde(default)]
    pub evidence_episode_ids: Vec<String>,
    #[serde(default)]
    pub pending_refs: Vec<u64>,
}

/// One retirement proposal: a profile fact the dream believes new episodes
/// contradict.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RetireProposal {
    pub section: String,
    pub key: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub evidence_episode_ids: Vec<String>,
}

/// The reply envelope: two arrays under named keys.
#[derive(Debug, serde::Deserialize)]
struct PromotionReply {
    #[serde(default)]
    promote: Vec<PromotionProposal>,
    #[serde(default)]
    retire: Vec<RetireProposal>,
}

/// A candidate that passed the channel checks, ready for the aggregate's own
/// adjudication. Evidence travels with it for the audit chain.
#[derive(Debug, Clone)]
pub struct AcceptedPromotion {
    pub observation: Observation,
    pub evidence_episode_ids: Vec<String>,
    pub pending_refs: Vec<u64>,
}

#[derive(Debug, Clone)]
pub struct AcceptedRetire {
    pub section: SectionKind,
    pub key: String,
    pub reason: String,
    pub evidence_episode_ids: Vec<String>,
}

/// A refused candidate (promotion or retirement): section/key plus the
/// refusal reason, rendered as an `UpgradeRefused` audit event.
#[derive(Debug, Clone)]
pub struct RefusedCandidate {
    pub section: String,
    pub key: String,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct PromotionPlan {
    pub accepted: Vec<AcceptedPromotion>,
    pub refused: Vec<RefusedCandidate>,
    pub retires: Vec<AcceptedRetire>,
}

/// Parse the dream reply: tolerate ```json fences and surrounding prose, take
/// the first JSON object.
pub fn parse_promotion_reply(reply: &str) -> Result<(Vec<PromotionProposal>, Vec<RetireProposal>)> {
    let stripped = reply.trim();
    let json_body = stripped
        .strip_prefix("```json")
        .or_else(|| stripped.strip_prefix("```"))
        .unwrap_or(stripped);
    let json_body = json_body.strip_suffix("```").unwrap_or(json_body).trim();
    let start = json_body
        .find('{')
        .context("promotion reply contains no JSON object")?;
    let end = json_body
        .rfind('}')
        .context("promotion reply object is not closed")?;
    let parsed: PromotionReply =
        serde_json::from_str(&json_body[start..=end]).context("promotion reply parse failed")?;
    Ok((parsed.promote, parsed.retire))
}

/// The pure channel check. `pending_count` is the highest valid pending line
/// number this pass may reference: pending entries are clues, never evidence,
/// so out-of-range refs are trimmed rather than refused.
pub fn plan_promotion(
    proposals: &[PromotionProposal],
    retires: &[RetireProposal],
    episodes: &[EpisodeRecord],
    pending_count: u64,
    profile: &UserProfile,
) -> PromotionPlan {
    let mut plan = PromotionPlan::default();
    let by_id: HashMap<&str, &EpisodeRecord> = episodes
        .iter()
        .map(|record| (record.id.as_str(), record))
        .collect();

    for proposal in proposals {
        let refused = |reason: &str| RefusedCandidate {
            section: proposal.section.clone(),
            key: proposal.key.clone(),
            reason: reason.to_owned(),
        };
        let Some(section) = SectionKind::parse(&proposal.section) else {
            plan.refused.push(refused("unknown section"));
            continue;
        };
        if section == SectionKind::Corrections {
            plan.refused
                .push(refused("corrections section is user-only (I-DR1)"));
            continue;
        }
        let kind = match proposal.kind.as_str() {
            "stable_fact" => ObservationKind::StableFact,
            "goal_signal" => ObservationKind::GoalSignal,
            _ => {
                plan.refused.push(refused("unknown kind"));
                continue;
            }
        };
        if proposal.evidence_episode_ids.is_empty() {
            plan.refused.push(refused("empty evidence chain"));
            continue;
        }
        let cited: Vec<&EpisodeRecord> = proposal
            .evidence_episode_ids
            .iter()
            .filter_map(|id| by_id.get(id.as_str()).copied())
            .collect();
        if cited.len() != proposal.evidence_episode_ids.len() {
            plan.refused.push(refused("unknown episode id"));
            continue;
        }
        if !cited.iter().any(|record| record.kind == "episode") {
            plan.refused.push(refused(
                "hypothesis-only evidence: agent inference cannot vouch for itself",
            ));
            continue;
        }
        let pending_refs: Vec<u64> = proposal
            .pending_refs
            .iter()
            .copied()
            .filter(|line| *line >= 1 && *line <= pending_count)
            .collect();
        plan.accepted.push(AcceptedPromotion {
            observation: Observation {
                section,
                key: proposal.key.clone(),
                value: proposal.value.clone(),
                kind,
                confidence: proposal.confidence,
            },
            evidence_episode_ids: proposal.evidence_episode_ids.clone(),
            pending_refs,
        });
    }

    for retire in retires {
        let refused = |reason: &str| RefusedCandidate {
            section: retire.section.clone(),
            key: retire.key.clone(),
            reason: reason.to_owned(),
        };
        let Some(section) = SectionKind::parse(&retire.section) else {
            plan.refused.push(refused("unknown section"));
            continue;
        };
        let Some(fact) = profile.fact(section, &retire.key) else {
            plan.refused.push(refused("retire target is absent"));
            continue;
        };
        if !matches!(fact.source, ChangeSource::AgentInference { .. }) {
            plan.refused
                .push(refused("retire target is user-sourced; untouchable"));
            continue;
        }
        plan.retires.push(AcceptedRetire {
            section,
            key: retire.key.clone(),
            reason: retire.reason.clone(),
            evidence_episode_ids: retire.evidence_episode_ids.clone(),
        });
    }

    plan
}

/// Build the one-call promotion prompt (Chinese, same voice as the
/// consolidation prompt): the task, the evidence red lines, the output
/// schema, the bounded episode window, the pending queue with its line
/// numbers, and the current profile for contradiction checks.
pub fn build_prompt(
    episodes: &[EpisodeRecord],
    pending: &[(u64, PendingObservation)],
    profile: &UserProfile,
) -> String {
    let mut prompt = String::new();
    prompt.push_str("你是记忆画像提升裁决器（夜间整理的第二阶段）。下面给出情节记录、待整理观察队列与当前用户画像。判断哪些内容可以被提升为画像事实，以及画像中哪些推断事实已被新情节推翻、应当退休。\n");
    prompt.push_str("\n证据红线（违反即作废）：\n");
    prompt.push_str(
        "1. 每条提升候选必须在 evidence_episode_ids 中引用至少一个 kind=episode 的情节 id；\n",
    );
    prompt.push_str("2. hypothesis（假设）与待整理队列里的观察都只是线索，永远不能单独作证，也不得互相佐证升级为事实；\n");
    prompt.push_str("3. 不得提议写入 corrections 分区（纠正只能来自用户本人）；\n");
    prompt.push_str("4. 退休提案只能针对画像中来源为 agent-inference 的事实，且必须在 evidence_episode_ids 中给出推翻它的情节 id；用户来源的事实永远不得提议退休。\n");
    prompt.push_str("\n规则：\n");
    prompt.push_str("1. kind 只能是 stable_fact 或 goal_signal；confidence 用 0-100；\n");
    prompt.push_str("2. pending_refs 填写该候选参考过的待整理观察行号（线索出处，可空）；\n");
    prompt.push_str("3. 没有值得提升或退休的内容时输出空数组，绝不凑数；\n");
    prompt.push_str("4. 只输出一个 JSON 对象：{\"promote\": [{\"section\": \"...\", \"key\": \"...\", \"value\": \"...\", \"kind\": \"stable_fact\", \"confidence\": 80, \"evidence_episode_ids\": [\"ep-...\"], \"pending_refs\": [3]}], \"retire\": [{\"section\": \"...\", \"key\": \"...\", \"reason\": \"...\", \"evidence_episode_ids\": [\"ep-...\"]}]}。\n");
    if episodes.is_empty() {
        prompt.push_str("\n情节记录：（空）\n");
    } else {
        prompt.push_str("\n情节记录：\n");
        for record in episodes {
            let text: String = record.text.chars().take(120).collect();
            prompt.push_str(&format!(
                "- [{}] ({}, {}) {}\n",
                record.id, record.kind, record.ts, text
            ));
        }
    }
    if pending.is_empty() {
        prompt.push_str("待整理观察：（空）\n");
    } else {
        prompt.push_str("待整理观察（行号即 pending_refs 引用编号）：\n");
        for (line, observation) in pending {
            let value: String = observation.value.chars().take(120).collect();
            prompt.push_str(&format!(
                "- [line {}] {}/{} = {} (confidence {})\n",
                line, observation.section, observation.key, value, observation.confidence
            ));
        }
    }
    prompt.push_str("当前画像：\n");
    let mut any_fact = false;
    for section in SectionKind::ALL {
        for (key, fact) in profile.section(section) {
            any_fact = true;
            let value: String = fact.value.chars().take(80).collect();
            let confidence = fact
                .source
                .confidence()
                .map(|c| format!(", confidence {c}"))
                .unwrap_or_default();
            prompt.push_str(&format!(
                "- [{}] {} = {} (source: {}{})\n",
                section.as_str(),
                key,
                value,
                fact.source.kind(),
                confidence
            ));
        }
    }
    if !any_fact {
        prompt.push_str("（空）\n");
    }
    prompt
}

/// The outcome of one promotion pass; consolidation's `RunSummary` carries it
/// and the audit chain renders it.
#[derive(Debug, Default)]
pub struct PromotionSummary {
    /// False when the pass was skipped for lack of material.
    pub ran: bool,
    pub llm_calls: u32,
    pub candidates: usize,
    pub written: usize,
    pub superseded: usize,
    pub refused: usize,
    pub retired: usize,
}

/// An episode is new material for the dream when its timestamp is newer than
/// the last successful promotion. An unparseable timestamp counts as new
/// (conservative: rather promote once too often than never).
fn episode_is_new(record: &EpisodeRecord, last_success_at: Option<&str>) -> bool {
    let Some(stamp) = last_success_at else {
        return true;
    };
    let Ok(stamp) = chrono::DateTime::parse_from_rfc3339(stamp) else {
        return true;
    };
    match chrono::DateTime::parse_from_rfc3339(&record.ts) {
        Ok(ts) => ts > stamp,
        Err(_) => true,
    }
}

/// One promotion pass. Assumes the consolidation lock is already held by the
/// caller (the dream is one process at a time). The no-material short path
/// runs BEFORE provider resolution so an empty night costs nothing.
pub async fn run_promotion(
    root: &Path,
    config: &zeroclaw_config::schema::Config,
    profile_service: &ProfileService,
) -> Result<PromotionSummary> {
    let events_file = dream_audit::events_path(root);
    let state_file = dream_audit::state_path(root);
    let state = dream_audit::load_state(&state_file)?.unwrap_or_default();
    let run_id = format!("dream-{}", Local::now().format("%Y%m%d-%H%M%S%.3f"));

    let store = EpisodeStore::new(episodes_path(root));
    let episodes = store.load_all()?;
    let queue = PendingQueue::new(pending_observations_path(root));
    let pending = queue.load_from(state.pending_cursor)?;

    let has_new_episodes = episodes
        .iter()
        .any(|record| episode_is_new(record, state.last_success_at.as_deref()));
    if !has_new_episodes && pending.is_empty() {
        dream_audit::append_event(
            &events_file,
            &DreamEvent::Skipped {
                run_id: run_id.clone(),
                reason: "no_material".to_owned(),
            },
        )?;
        dream_audit::save_state(
            &state_file,
            &DreamState {
                last_run_id: Some(run_id),
                last_success_at: Some(Local::now().to_rfc3339()),
                pending_cursor: state.pending_cursor,
            },
        )?;
        return Ok(PromotionSummary::default());
    }

    match run_promotion_inner(root, config, profile_service, &run_id, &state).await {
        Ok(summary) => Ok(summary),
        Err(error) => {
            // The failure itself is auditable (I-DR3); the episodes stage's
            // cursor stays untouched — its writes are durable and idempotent.
            let _ = dream_audit::append_event(
                &events_file,
                &DreamEvent::RunFailed {
                    run_id,
                    finished_at: Local::now().to_rfc3339(),
                    error: format!("{error:#}"),
                },
            );
            Err(error)
        }
    }
}

async fn run_promotion_inner(
    root: &Path,
    config: &zeroclaw_config::schema::Config,
    profile_service: &ProfileService,
    run_id: &str,
    state: &DreamState,
) -> Result<PromotionSummary> {
    let events_file = dream_audit::events_path(root);
    let state_file = dream_audit::state_path(root);
    let started_at = Local::now().to_rfc3339();
    dream_audit::append_event(
        &events_file,
        &DreamEvent::RunStarted {
            run_id: run_id.to_owned(),
            trigger: "consolidation".to_owned(),
            started_at,
        },
    )?;

    let provider_ref = config
        .agents
        .get("kangaroo")
        .map(|agent| agent.model_provider.to_string())
        .filter(|reference| !reference.is_empty())
        .context("promotion needs [agents.kangaroo].model_provider")?;

    let episodes = EpisodeStore::new(episodes_path(root)).load_all()?;
    let queue = PendingQueue::new(pending_observations_path(root));
    let pending = queue.load_from(state.pending_cursor)?;
    let queue_len = queue.len()?;
    let profile = profile_service.current()?.state.unwrap_or_default();

    // Prompt windows: the most recent episodes, the oldest unconsumed
    // pending entries.
    let episode_window: Vec<EpisodeRecord> = episodes
        .iter()
        .rev()
        .take(PROMPT_EPISODE_LIMIT)
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let pending_window: Vec<(u64, PendingObservation)> =
        pending.iter().take(PROMPT_PENDING_LIMIT).cloned().collect();
    let prompt = build_prompt(&episode_window, &pending_window, &profile);

    let mut summary = PromotionSummary {
        ran: true,
        ..Default::default()
    };
    let mut reply_text = String::new();
    while summary.llm_calls < MAX_PROMOTION_CALLS_PER_PASS {
        let resolved =
            zeroclaw_providers::create_model_provider_from_ref_with_model(config, &provider_ref)?;
        let model = resolved
            .model
            .clone()
            .context("provider alias carries no model for promotion")?;
        let reply = resolved
            .provider
            .chat(
                ChatRequest {
                    messages: &[ChatMessage::user(prompt.clone())],
                    tools: None,
                    thinking: None,
                },
                &model,
                Some(0.2),
            )
            .await
            .context("promotion LLM call failed")?;
        summary.llm_calls += 1;
        reply_text = reply.text_or_empty().to_owned();
    }

    let (proposals, retires) = parse_promotion_reply(&reply_text)?;
    for proposal in &proposals {
        dream_audit::append_event(
            &events_file,
            &DreamEvent::Candidate {
                run_id: run_id.to_owned(),
                section: proposal.section.clone(),
                key: proposal.key.clone(),
                value: proposal.value.clone(),
                confidence: proposal.confidence,
                episode_ids: proposal.evidence_episode_ids.clone(),
                pending_refs: proposal.pending_refs.clone(),
            },
        )?;
    }
    summary.candidates = proposals.len();
    let plan = plan_promotion(&proposals, &retires, &episodes, queue_len, &profile);

    for refused in &plan.refused {
        dream_audit::append_event(
            &events_file,
            &DreamEvent::UpgradeRefused {
                run_id: run_id.to_owned(),
                section: refused.section.clone(),
                key: refused.key.clone(),
                reason: refused.reason.clone(),
            },
        )?;
    }
    summary.refused = plan.refused.len();

    // Legal candidates go through the aggregate's own adjudication in
    // per-turn-budget-sized batches; precedence refusals stay observable.
    let accepted: Vec<Observation> = plan
        .accepted
        .iter()
        .map(|promotion| promotion.observation.clone())
        .collect();
    for batch in accepted.chunks(MAX_PROFILE_UPDATES_PER_TURN) {
        let recording = profile_service.observe_batch(batch)?;
        if !recording.patches.is_empty() {
            let fact_keys: Vec<String> = recording
                .outcomes
                .iter()
                .filter(|outcome| matches!(outcome.decision, RecordDecision::Record))
                .map(|outcome| format!("{}.{}", outcome.section.as_str(), outcome.key))
                .collect();
            dream_audit::append_event(
                &events_file,
                &DreamEvent::Committed {
                    run_id: run_id.to_owned(),
                    fact_keys,
                },
            )?;
        }
        for outcome in &recording.outcomes {
            match outcome.decision {
                RecordDecision::Record => summary.written += 1,
                RecordDecision::Ignore {
                    reason: crate::domain::profile::IgnoreReason::Superseded,
                } => {
                    summary.superseded += 1;
                    dream_audit::append_event(
                        &events_file,
                        &DreamEvent::Superseded {
                            run_id: run_id.to_owned(),
                            section: outcome.section.as_str().to_owned(),
                            key: outcome.key.clone(),
                            reason: "kept the existing higher-priority or newer fact".to_owned(),
                        },
                    )?;
                }
                // Duplicate/low-confidence/etc. declines are idempotent
                // no-ops (replayed material), deliberately unaudited.
                _ => {}
            }
        }
    }

    for retire in &plan.retires {
        dream_audit::append_event(
            &events_file,
            &DreamEvent::RetireProposed {
                run_id: run_id.to_owned(),
                section: retire.section.as_str().to_owned(),
                key: retire.key.clone(),
                reason: retire.reason.clone(),
                episode_ids: retire.evidence_episode_ids.clone(),
            },
        )?;
    }
    if !plan.retires.is_empty() {
        let targets: Vec<(SectionKind, String)> = plan
            .retires
            .iter()
            .map(|retire| (retire.section, retire.key.clone()))
            .collect();
        summary.retired =
            profile_service.retire_inferred_facts(&targets, &format!("dream {run_id}"))?;
    }

    // The pending cursor advances past exactly the entries the prompt carried;
    // anything beyond the window waits for the next pass.
    let pending_cursor = pending_window
        .last()
        .map(|(line, _)| *line)
        .unwrap_or(state.pending_cursor);
    dream_audit::append_event(
        &events_file,
        &DreamEvent::RunFinished {
            run_id: run_id.to_owned(),
            finished_at: Local::now().to_rfc3339(),
            candidates: summary.candidates,
            written: summary.written,
            retired: summary.retired,
            refused: summary.refused,
            superseded: summary.superseded,
        },
    )?;
    dream_audit::save_state(
        &state_file,
        &DreamState {
            last_run_id: Some(run_id.to_owned()),
            last_success_at: Some(Local::now().to_rfc3339()),
            pending_cursor,
        },
    )?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::profile::ProfileFact;

    fn episode(id: &str, kind: &str) -> EpisodeRecord {
        EpisodeRecord {
            id: id.to_owned(),
            ts: "2026-11-04T12:00:00+08:00".to_owned(),
            text: format!("text of {id}"),
            kind: kind.to_owned(),
            trace: crate::infra::episodes::EpisodeTrace {
                file: "2026-11-04.jsonl".to_owned(),
                msg_id: "m1".to_owned(),
                line: 1,
            },
            confidence: 80,
            source: if kind == "hypothesis" {
                "AgentInference".to_owned()
            } else {
                "Consolidation".to_owned()
            },
        }
    }

    fn proposal(section: &str, kind: &str, evidence: &[&str]) -> PromotionProposal {
        PromotionProposal {
            section: section.to_owned(),
            key: "editor".to_owned(),
            value: "Neovim".to_owned(),
            kind: kind.to_owned(),
            confidence: 80,
            evidence_episode_ids: evidence.iter().map(|id| id.to_string()).collect(),
            pending_refs: Vec::new(),
        }
    }

    fn profile_with(section: SectionKind, key: &str, source: ChangeSource) -> UserProfile {
        let mut profile = UserProfile::default();
        profile.section_mut(section).insert(
            key.to_owned(),
            ProfileFact {
                value: "existing".to_owned(),
                source,
                updated_at: 1,
            },
        );
        profile
    }

    #[test]
    fn empty_evidence_is_refused() {
        let plan = plan_promotion(
            &[proposal("preferences", "stable_fact", &[])],
            &[],
            &[episode("ep-1", "episode")],
            0,
            &UserProfile::default(),
        );
        assert_eq!(plan.accepted.len(), 0);
        assert_eq!(plan.refused.len(), 1);
        assert!(plan.refused[0].reason.contains("empty evidence"));
    }

    #[test]
    fn unknown_episode_id_is_refused() {
        let plan = plan_promotion(
            &[proposal("preferences", "stable_fact", &["ep-ghost"])],
            &[],
            &[episode("ep-1", "episode")],
            0,
            &UserProfile::default(),
        );
        assert_eq!(plan.refused.len(), 1);
        assert!(plan.refused[0].reason.contains("unknown episode id"));
    }

    #[test]
    fn hypothesis_only_evidence_is_refused() {
        let episodes = vec![
            episode("ep-h1", "hypothesis"),
            episode("ep-h2", "hypothesis"),
        ];
        let plan = plan_promotion(
            &[proposal("preferences", "stable_fact", &["ep-h1", "ep-h2"])],
            &[],
            &episodes,
            0,
            &UserProfile::default(),
        );
        assert_eq!(plan.refused.len(), 1);
        assert!(plan.refused[0].reason.contains("hypothesis-only"));
        // One real episode among the cited ids redeems the candidate.
        let episodes = vec![episode("ep-h1", "hypothesis"), episode("ep-2", "episode")];
        let plan = plan_promotion(
            &[proposal("preferences", "stable_fact", &["ep-h1", "ep-2"])],
            &[],
            &episodes,
            0,
            &UserProfile::default(),
        );
        assert_eq!(plan.accepted.len(), 1);
    }

    #[test]
    fn corrections_section_is_refused() {
        let plan = plan_promotion(
            &[proposal("corrections", "stable_fact", &["ep-1"])],
            &[],
            &[episode("ep-1", "episode")],
            0,
            &UserProfile::default(),
        );
        assert_eq!(plan.refused.len(), 1);
        assert!(plan.refused[0].reason.contains("corrections"));
    }

    #[test]
    fn unknown_section_and_kind_are_refused() {
        let plan = plan_promotion(
            &[
                proposal("nonsense", "stable_fact", &["ep-1"]),
                proposal("preferences", "correction", &["ep-1"]),
            ],
            &[],
            &[episode("ep-1", "episode")],
            0,
            &UserProfile::default(),
        );
        assert_eq!(plan.accepted.len(), 0);
        assert_eq!(plan.refused.len(), 2);
        assert!(plan.refused[0].reason.contains("unknown section"));
        assert!(plan.refused[1].reason.contains("unknown kind"));
    }

    #[test]
    fn a_legal_candidate_maps_to_a_domain_observation() {
        let mut goal = proposal("goals", "goal_signal", &["ep-1"]);
        goal.key = "trip".to_owned();
        goal.confidence = 65;
        let episodes = vec![episode("ep-1", "episode")];
        let plan = plan_promotion(&[goal], &[], &episodes, 0, &UserProfile::default());
        assert_eq!(plan.refused.len(), 0);
        let accepted = &plan.accepted[0];
        assert_eq!(accepted.observation.section, SectionKind::Goals);
        assert!(matches!(
            accepted.observation.kind,
            ObservationKind::GoalSignal
        ));
        assert_eq!(accepted.observation.confidence, 65);
        assert_eq!(accepted.evidence_episode_ids, vec!["ep-1".to_owned()]);
    }

    #[test]
    fn out_of_range_pending_refs_are_trimmed_not_refused() {
        let mut candidate = proposal("preferences", "stable_fact", &["ep-1"]);
        candidate.pending_refs = vec![0, 2, 99];
        let episodes = vec![episode("ep-1", "episode")];
        let plan = plan_promotion(&[candidate], &[], &episodes, 3, &UserProfile::default());
        assert_eq!(plan.accepted.len(), 1);
        assert_eq!(plan.accepted[0].pending_refs, vec![2]);
    }

    #[test]
    fn retire_is_accepted_only_for_agent_inference_facts() {
        let retire = |section: &str, key: &str| RetireProposal {
            section: section.to_owned(),
            key: key.to_owned(),
            reason: "contradicted".to_owned(),
            evidence_episode_ids: vec!["ep-1".to_owned()],
        };
        let mut profile = profile_with(
            SectionKind::Preferences,
            "inferred",
            ChangeSource::AgentInference { confidence: 70 },
        );
        profile.section_mut(SectionKind::Preferences).insert(
            "explicit".to_owned(),
            ProfileFact {
                value: "user's".to_owned(),
                source: ChangeSource::UserExplicitEdit,
                updated_at: 1,
            },
        );
        profile.section_mut(SectionKind::Goals).insert(
            "corrected".to_owned(),
            ProfileFact {
                value: "user said".to_owned(),
                source: ChangeSource::UserConversationCorrection,
                updated_at: 1,
            },
        );
        let plan = plan_promotion(
            &[],
            &[
                retire("preferences", "inferred"),
                retire("preferences", "explicit"),
                retire("goals", "corrected"),
                retire("goals", "absent"),
                retire("nonsense", "x"),
            ],
            &[episode("ep-1", "episode")],
            0,
            &profile,
        );
        assert_eq!(plan.retires.len(), 1);
        assert_eq!(plan.retires[0].key, "inferred");
        assert_eq!(plan.refused.len(), 4);
        assert!(plan.refused[0].reason.contains("user-sourced"));
        assert!(plan.refused[1].reason.contains("user-sourced"));
        assert!(plan.refused[2].reason.contains("absent"));
        assert!(plan.refused[3].reason.contains("unknown section"));
    }

    #[test]
    fn parse_tolerates_fences_and_prose() {
        let reply = "好的：\n```json\n{\"promote\": [{\"section\": \"preferences\", \"key\": \"editor\", \"value\": \"Neovim\", \"kind\": \"stable_fact\", \"confidence\": 80, \"evidence_episode_ids\": [\"ep-1\"]}], \"retire\": []}\n```";
        let (promote, retire) = parse_promotion_reply(reply).expect("parse");
        assert_eq!(promote.len(), 1);
        assert_eq!(promote[0].evidence_episode_ids, vec!["ep-1".to_owned()]);
        assert!(retire.is_empty());
        assert!(parse_promotion_reply("no object here").is_err());
        // Missing arrays default to empty.
        let (promote, _) = parse_promotion_reply("{}").expect("empty reply");
        assert!(promote.is_empty());
    }

    #[test]
    fn prompt_marks_hypotheses_and_pending_as_clues_only() {
        let episodes = vec![episode("ep-1", "episode"), episode("ep-h1", "hypothesis")];
        let pending = vec![(
            3u64,
            PendingObservation {
                section: "preferences".to_owned(),
                key: "editor".to_owned(),
                value: "Neovim".to_owned(),
                confidence: 70,
                enqueued_at: "2026-11-05T03:00:00+08:00".to_owned(),
            },
        )];
        let prompt = build_prompt(&episodes, &pending, &UserProfile::default());
        assert!(prompt.contains("线索"), "clue wording present");
        assert!(prompt.contains("永远不能单独作证"));
        assert!(prompt.contains("ep-1"));
        assert!(prompt.contains("ep-h1"));
        assert!(prompt.contains("[line 3]"));
        assert!(prompt.contains("hypothesis"));
    }

    #[test]
    fn prompt_renders_profile_facts_with_sources() {
        let profile = profile_with(
            SectionKind::Preferences,
            "editor",
            ChangeSource::AgentInference { confidence: 70 },
        );
        let prompt = build_prompt(&[], &[], &profile);
        assert!(
            prompt.contains(
                "[preferences] editor = existing (source: agent-inference, confidence 70)"
            )
        );
        assert!(prompt.contains("情节记录：（空）"));
        assert!(prompt.contains("待整理观察：（空）"));
    }

    #[tokio::test]
    async fn no_material_skips_without_a_provider() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let profile_store = crate::infra::markdown_profile_store(root).expect("store");
        let profile_service = ProfileService::new(profile_store, crate::infra::system_clock());
        // A config with no agents at all: provider resolution would fail if
        // the pass tried to reach it.
        let config = zeroclaw_config::schema::Config::default();
        let summary = run_promotion(root, &config, &profile_service)
            .await
            .expect("no-material pass succeeds");
        assert!(!summary.ran);
        assert_eq!(summary.llm_calls, 0);
        let raw = std::fs::read_to_string(dream_audit::events_path(root)).expect("events");
        assert!(raw.contains("\"event\":\"skipped\""));
        assert!(raw.contains("no_material"));
        let state = dream_audit::load_state(&dream_audit::state_path(root))
            .expect("state")
            .expect("state written");
        assert_eq!(state.pending_cursor, 0);
        assert!(state.last_success_at.is_some());
    }

    #[test]
    fn episode_novelty_compares_against_last_success() {
        let old = episode("ep-old", "episode");
        let mut new_record = episode("ep-new", "episode");
        new_record.ts = "2026-11-05T03:30:00+08:00".to_owned();
        let stamp = Some("2026-11-05T03:00:00+08:00");
        assert!(!episode_is_new(&old, stamp));
        assert!(episode_is_new(&new_record, stamp));
        assert!(episode_is_new(&old, None), "never ran: everything is new");
        let mut broken = episode("ep-broken", "episode");
        broken.ts = "not a date".to_owned();
        assert!(
            episode_is_new(&broken, stamp),
            "unparseable ts counts as new"
        );
    }
}
