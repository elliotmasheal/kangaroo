//! UserProfile use cases: observation intake behind the update criterion,
//! per-turn projection, and rollback.

use anyhow::Result;
use std::sync::Arc;
use zeroclaw_log::{Action, Event, record};

use crate::application::ports::{Clock, CommitReceipt, LoadedDoc, ProfileDocumentStore};
use crate::domain::profile::{
    FactPatch, Observation, ObservationKind, PatchAction, RecordDecision, SectionKind, UserProfile,
    WorthRecording,
};
use crate::domain::{ChangeKind, RevisionId};

/// Per-turn update budget (invariant I-U5): observations beyond this count
/// that would otherwise be recorded become `NeedsReview` instead.
pub const MAX_PROFILE_UPDATES_PER_TURN: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileProjection {
    pub fact_count: usize,
    /// Markdown context for the turn; facts plus the user's preserved notes.
    pub context: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationOutcome {
    pub section: SectionKind,
    pub key: String,
    pub decision: RecordDecision,
}

/// UserProfile use cases. Writes go through the profile store only; the Self
/// document is never touched from here (I-X1).
pub struct ProfileService {
    store: Arc<dyn ProfileDocumentStore>,
    clock: Arc<dyn Clock>,
    policy: WorthRecording,
}

impl ProfileService {
    pub fn new(store: Arc<dyn ProfileDocumentStore>, clock: Arc<dyn Clock>) -> Self {
        Self {
            store,
            clock,
            policy: WorthRecording,
        }
    }

    /// Fresh load; surfaces external-edit handling (the user's hand edit is
    /// absorbed here, invariant I-U2).
    pub fn current(&self) -> Result<LoadedDoc<UserProfile>> {
        self.store.load()
    }

    /// Applies the update criterion to a batch of observations and commits
    /// the recorded ones as one revision. Budget: at most
    /// `MAX_PROFILE_UPDATES_PER_TURN` recorded observations per call;
    /// overflow becomes `NeedsReview` (kept as observation, not written).
    pub fn observe_batch(&self, observations: &[Observation]) -> Result<Vec<ObservationOutcome>> {
        let loaded = self.store.load()?;
        let mut state = loaded.state.clone().unwrap_or_default();
        let mut patches: Vec<FactPatch> = Vec::new();
        let mut outcomes = Vec::with_capacity(observations.len());
        for observation in observations {
            let existing = state.fact(observation.section, &observation.key).cloned();
            let decision = self.policy.decide(observation, existing.as_ref());
            let decision = match decision {
                RecordDecision::Record if patches.len() >= MAX_PROFILE_UPDATES_PER_TURN => {
                    RecordDecision::NeedsReview
                }
                other => other,
            };
            if let RecordDecision::Record = decision {
                patches.push(FactPatch {
                    section: observation.section,
                    key: observation.key.clone(),
                    action: PatchAction::Upsert {
                        value: observation.value.clone(),
                        source: source_for(observation),
                    },
                });
            }
            outcomes.push(ObservationOutcome {
                section: observation.section,
                key: observation.key.clone(),
                decision,
            });
        }
        if !patches.is_empty() {
            for patch in &patches {
                state.apply_patch(patch, self.clock.now_ms());
            }
            let receipt = self.store.commit(
                loaded.fingerprint,
                &state,
                ChangeKind::Experience,
                "profile observations recorded",
                self.clock.now_ms(),
            )?;
            record!(
                INFO,
                Event::new(module_path!(), Action::Write).with_attrs(serde_json::json!({
                    "revision": receipt.revision.0,
                    "facts": patches.len(),
                })),
                "profile facts committed from observations"
            );
        }
        Ok(outcomes)
    }

    /// Per-turn profile projection: reads the document now, no caching.
    pub fn projection(&self) -> Result<ProfileProjection> {
        let loaded = self.store.load()?;
        let Some(state) = loaded.state else {
            return Ok(ProfileProjection {
                fact_count: 0,
                context: String::new(),
            });
        };
        let mut context = String::new();
        for (kind, title) in [
            (SectionKind::Identity, "Identity"),
            (SectionKind::Environment, "Environment"),
            (SectionKind::Preferences, "Preferences"),
            (SectionKind::Goals, "Goals"),
            (SectionKind::Corrections, "Corrections"),
        ] {
            let facts = state.section(kind);
            if facts.is_empty() {
                continue;
            }
            context.push_str(&format!("\n## {title}\n"));
            for (key, fact) in facts {
                context.push_str(&format!("- {key}: {}\n", fact.value));
            }
        }
        if !state.notes.trim().is_empty() {
            context.push_str(&format!("\n## Notes (user's own)\n{}\n", state.notes));
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

fn source_for(observation: &Observation) -> crate::domain::profile::ChangeSource {
    match observation.kind {
        ObservationKind::Correction => {
            crate::domain::profile::ChangeSource::UserConversationCorrection
        }
        _ => crate::domain::profile::ChangeSource::AgentInference {
            confidence: observation.confidence,
        },
    }
}
