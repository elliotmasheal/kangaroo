//! UserProfile use cases: observation intake, per-turn projection, and
//! rollback. The application layer only orchestrates — the update criterion,
//! the per-turn budget, and the provenance mapping are domain rules that
//! live on the aggregate (`UserProfile::plan_recording`).

use anyhow::Result;
use std::sync::Arc;
use zeroclaw_log::{Action, Event, record};

use crate::application::ports::{Clock, CommitReceipt, LoadedDoc, ProfileDocumentStore};
use crate::domain::profile::{RecordingPlan, SectionKind, UserProfile};
use crate::domain::{ChangeKind, RevisionId};

/// Per-turn update budget (invariant I-U5). The budget is a policy input the
/// application passes into the aggregate's planning; the rule itself is
/// enforced by `UserProfile::plan_recording`.
pub const MAX_PROFILE_UPDATES_PER_TURN: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileProjection {
    pub fact_count: usize,
    /// Markdown context for the turn; facts plus the user's preserved notes.
    pub context: String,
}

/// UserProfile use cases. Writes go through the profile store only; the Self
/// document is never touched from here (I-X1).
pub struct ProfileService {
    store: Arc<dyn ProfileDocumentStore>,
    clock: Arc<dyn Clock>,
}

impl ProfileService {
    pub fn new(store: Arc<dyn ProfileDocumentStore>, clock: Arc<dyn Clock>) -> Self {
        Self { store, clock }
    }

    /// Fresh load; surfaces external-edit handling (the user's hand edit is
    /// absorbed here, invariant I-U2).
    pub fn current(&self) -> Result<LoadedDoc<UserProfile>> {
        self.store.load()
    }

    /// Applies the aggregate's recording plan to a batch of observations and
    /// commits the planned patches as one revision. The commit note records
    /// every displaced fact (ADR-SA-002 §3: 失效而非删除).
    pub fn observe_batch(
        &self,
        observations: &[crate::domain::profile::Observation],
    ) -> Result<RecordingPlan> {
        let loaded = self.store.load()?;
        let mut state = loaded.state.clone().unwrap_or_default();
        let now = self.clock.now_ms();
        let plan = state.plan_recording(observations, MAX_PROFILE_UPDATES_PER_TURN, now);
        if !plan.patches.is_empty() {
            state.apply_patches(&plan.patches, now);
            let note = if plan.overrides.is_empty() {
                "profile observations recorded".to_owned()
            } else {
                format!(
                    "profile observations recorded; superseded: {}",
                    plan.overrides.join("; ")
                )
            };
            let receipt = self.store.commit(
                loaded.fingerprint,
                &state,
                ChangeKind::Experience,
                &note,
                now,
            )?;
            record!(
                INFO,
                Event::new(module_path!(), Action::Write).with_attrs(serde_json::json!({
                    "revision": receipt.revision.0,
                    "facts": plan.patches.len(),
                    "superseded": plan.overrides.len(),
                })),
                "profile facts committed from observations"
            );
        }
        Ok(plan)
    }

    /// Per-turn profile projection: reads the document now, no caching.
    /// Rendering is budgeted (ADR-SA-003): sections fill in the truncation
    /// order `Corrections > Identity > Preferences > Goals > Environment`,
    /// within a section the newest facts come first, and facts that do not
    /// fit are counted into the audit log instead of being silently dropped.
    pub fn projection(&self) -> Result<ProfileProjection> {
        const BUDGET_CHARS: usize = 2_000;
        let loaded = self.store.load()?;
        let Some(state) = loaded.state else {
            return Ok(ProfileProjection {
                fact_count: 0,
                context: String::new(),
            });
        };
        let order = [
            SectionKind::Corrections,
            SectionKind::Identity,
            SectionKind::Preferences,
            SectionKind::Goals,
            SectionKind::Environment,
        ];
        let mut context = String::new();
        let mut budget = BUDGET_CHARS;
        let mut omitted = 0usize;
        for kind in order {
            let mut facts: Vec<(&String, &crate::domain::profile::ProfileFact)> =
                state.section(kind).iter().collect();
            facts.sort_by_key(|(_, fact)| std::cmp::Reverse(fact.updated_at));
            if facts.is_empty() {
                continue;
            }
            let section_header = format!("\n## {}\n", kind.title());
            if section_header.len() > budget {
                omitted += facts.len();
                continue;
            }
            context.push_str(&section_header);
            budget -= section_header.len();
            for (key, fact) in facts {
                let line = format!("- {key}: {}\n", fact.value);
                if line.len() > budget {
                    omitted += 1;
                    continue;
                }
                context.push_str(&line);
                budget -= line.len();
            }
        }
        if !state.notes.trim().is_empty() {
            let notes = format!("\n## Notes (user's own)\n{}\n", state.notes);
            if notes.len() <= budget {
                context.push_str(&notes);
            } else {
                omitted += 1;
            }
        }
        if omitted > 0 {
            record!(
                INFO,
                Event::new(module_path!(), Action::Skip).with_attrs(serde_json::json!({
                    "omitted": omitted,
                    "budget_chars": BUDGET_CHARS,
                })),
                "profile projection truncated to the context budget"
            );
        }
        Ok(ProfileProjection {
            fact_count: state.fact_count(),
            context,
        })
    }

    /// Rollback to an earlier revision (invariant I-U7); the rollback itself
    /// is a new revision.
    pub fn rollback(&self, to: RevisionId) -> Result<CommitReceipt> {
        let loaded = self.store.load()?;
        let receipt = self
            .store
            .rollback(loaded.fingerprint, to, self.clock.now_ms())?;
        record!(
            INFO,
            Event::new(module_path!(), Action::Write).with_attrs(serde_json::json!({
                "revision": receipt.revision.0,
                "rolled_back_to": to.0,
            })),
            "user profile rolled back"
        );
        Ok(receipt)
    }
}
