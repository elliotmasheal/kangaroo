//! The UserProfile aggregate: structured facts about the user. This is the
//! only document a user may hand-edit (decision: asymmetric edit rights);
//! the Self document is restored, never absorbed.

use std::collections::BTreeMap;

use crate::domain::TimestampMs;

/// The five fixed profile sections. A fixed set keeps machine merging and
/// user edits diffable. String forms are persistence vocabulary owned by
/// `infra`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SectionKind {
    Identity,
    Environment,
    Preferences,
    Goals,
    Corrections,
}

impl SectionKind {
    pub const ALL: [SectionKind; 5] = [
        Self::Identity,
        Self::Environment,
        Self::Preferences,
        Self::Goals,
        Self::Corrections,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Environment => "environment",
            Self::Preferences => "preferences",
            Self::Goals => "goals",
            Self::Corrections => "corrections",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "identity" => Self::Identity,
            "environment" => Self::Environment,
            "preferences" => Self::Preferences,
            "goals" => Self::Goals,
            "corrections" => Self::Corrections,
            _ => return None,
        })
    }

    pub const fn title(self) -> &'static str {
        match self {
            Self::Identity => "Identity",
            Self::Environment => "Environment",
            Self::Preferences => "Preferences",
            Self::Goals => "Goals",
            Self::Corrections => "Corrections",
        }
    }
}

/// Facts within one section, keyed by a free-form key.
pub type SectionFacts = BTreeMap<String, ProfileFact>;

/// Where a fact came from; drives the override priority (invariant I-U4:
/// user explicit edits outrank conversation corrections outrank inference).
/// String forms are persistence vocabulary owned by `infra`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeSource {
    UserExplicitEdit,
    UserConversationCorrection,
    AgentInference { confidence: u8 },
}

impl ChangeSource {
    /// The stable kind token; `AgentInference` additionally carries its
    /// confidence as a separate field in persistence.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::UserExplicitEdit => "user-explicit-edit",
            Self::UserConversationCorrection => "user-conversation-correction",
            Self::AgentInference { .. } => "agent-inference",
        }
    }

    /// The per-kind confidence, when the source carries one.
    pub fn confidence(&self) -> Option<u8> {
        match self {
            Self::AgentInference { confidence } => Some(*confidence),
            _ => None,
        }
    }

    pub fn from_parts(kind: &str, confidence: Option<u8>) -> Result<Self, String> {
        Ok(match kind {
            "user-explicit-edit" => {
                if confidence.is_some() {
                    return Err(format!(
                        "the {kind} source does not carry a confidence value"
                    ));
                }
                Self::UserExplicitEdit
            }
            "user-conversation-correction" => {
                if confidence.is_some() {
                    return Err(format!(
                        "the {kind} source does not carry a confidence value"
                    ));
                }
                Self::UserConversationCorrection
            }
            "agent-inference" => Self::AgentInference {
                confidence: confidence
                    .ok_or_else(|| format!("the {kind} source requires a confidence value"))?,
            },
            other => return Err(format!("unknown change source kind: {other}")),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileFact {
    pub value: String,
    pub source: ChangeSource,
    pub updated_at: TimestampMs,
}

/// Outcome of the conflict adjudication between an existing fact and an
/// incoming one (ADR-SA-002): a pure decision, no IO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupersedeDecision {
    /// The incoming fact replaces the existing one.
    Override,
    /// The existing fact stays; the incoming one is not written.
    Keep,
}

/// Source priority for conflict adjudication (invariant I-U4, ADR-SA-002):
/// user explicit edits outrank user corrections outrank agent inference.
/// Confidence deliberately does NOT participate — an agent's self-assessed
/// confidence must never outrank the user (echo-chamber guard).
fn source_rank(source: &ChangeSource) -> u8 {
    match source {
        ChangeSource::UserExplicitEdit => 3,
        ChangeSource::UserConversationCorrection => 2,
        ChangeSource::AgentInference { .. } => 1,
    }
}

impl ProfileFact {
    /// Adjudicates whether `incoming` (with its source and timestamp) may
    /// replace this fact. Same priority resolves by recency: the newer
    /// timestamp wins. Pure function (ADR-SA-002).
    pub fn supersede(
        &self,
        incoming_source: &ChangeSource,
        incoming_at: TimestampMs,
    ) -> SupersedeDecision {
        let (existing_rank, incoming_rank) =
            (source_rank(&self.source), source_rank(incoming_source));
        if incoming_rank > existing_rank {
            SupersedeDecision::Override
        } else if incoming_rank < existing_rank {
            SupersedeDecision::Keep
        } else if incoming_at >= self.updated_at {
            SupersedeDecision::Override
        } else {
            SupersedeDecision::Keep
        }
    }
}

/// The UserProfile aggregate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserProfile {
    pub identity: SectionFacts,
    pub environment: SectionFacts,
    pub preferences: SectionFacts,
    pub goals: SectionFacts,
    pub corrections: SectionFacts,
    /// Free-form narrative area preserved verbatim on every rewrite; the
    /// user's own notes live here.
    pub notes: String,
}

impl UserProfile {
    pub fn section(&self, kind: SectionKind) -> &SectionFacts {
        match kind {
            SectionKind::Identity => &self.identity,
            SectionKind::Environment => &self.environment,
            SectionKind::Preferences => &self.preferences,
            SectionKind::Goals => &self.goals,
            SectionKind::Corrections => &self.corrections,
        }
    }

    pub fn section_mut(&mut self, kind: SectionKind) -> &mut SectionFacts {
        match kind {
            SectionKind::Identity => &mut self.identity,
            SectionKind::Environment => &mut self.environment,
            SectionKind::Preferences => &mut self.preferences,
            SectionKind::Goals => &mut self.goals,
            SectionKind::Corrections => &mut self.corrections,
        }
    }

    pub fn fact(&self, kind: SectionKind, key: &str) -> Option<&ProfileFact> {
        self.section(kind).get(key)
    }

    /// Applies a batch of fact patches (I-U4: fact-level, never a section
    /// rewrite).
    pub fn apply_patches(&mut self, patches: &[FactPatch], at: TimestampMs) {
        for patch in patches {
            let section = self.section_mut(patch.section);
            match &patch.action {
                PatchAction::Upsert { value, source } => {
                    section.insert(
                        patch.key.clone(),
                        ProfileFact {
                            value: value.clone(),
                            source: source.clone(),
                            updated_at: at,
                        },
                    );
                }
                PatchAction::Remove => {
                    section.remove(&patch.key);
                }
            }
        }
    }

    /// The update criterion for one observation (invariant I-U3). Business
    /// rule: this lives on the aggregate, not on the application service.
    pub fn evaluate_observation(&self, observation: &Observation) -> RecordDecision {
        if observation.value.trim().is_empty() {
            return RecordDecision::Ignore {
                reason: IgnoreReason::EmptyValue,
            };
        }
        if matches!(observation.kind, ObservationKind::Transient) {
            return RecordDecision::Ignore {
                reason: IgnoreReason::Transient,
            };
        }
        if is_credential_like(&observation.key, &observation.value) {
            return RecordDecision::Ignore {
                reason: IgnoreReason::CredentialLike,
            };
        }
        if matches!(observation.kind, ObservationKind::Correction) {
            return RecordDecision::Record;
        }
        if observation.confidence < MIN_CONFIDENCE {
            return RecordDecision::Ignore {
                reason: IgnoreReason::LowConfidence,
            };
        }
        if self
            .fact(observation.section, &observation.key)
            .is_some_and(|fact| fact.value == observation.value)
        {
            return RecordDecision::Ignore {
                reason: IgnoreReason::Duplicate,
            };
        }
        RecordDecision::Record
    }

    /// Plans the recording for a batch of observations against the per-turn
    /// budget (invariant I-U5) and each section's fact capacity (ADR-SA-003
    /// decision 4): the first `budget` recordable observations become
    /// patches, a section at capacity refuses new facts until one is
    /// retired, and both refusals stay observable outcomes that are never
    /// written. The rules live here, on the aggregate; callers only commit
    /// what the plan contains.
    pub fn plan_recording(
        &self,
        observations: &[Observation],
        budget: usize,
        at: TimestampMs,
    ) -> RecordingPlan {
        let mut patches = Vec::new();
        let mut outcomes = Vec::with_capacity(observations.len());
        let mut overrides = Vec::new();
        // Adjudicate against the aggregate state AND the patches already
        // planned within this batch (later observations in one batch race
        // the earlier ones on the same key); the batch timestamp feeds the
        // recency arm of the adjudication.
        let mut staged: BTreeMap<(SectionKind, String), (ChangeSource, TimestampMs)> =
            BTreeMap::new();
        // Per-section occupancy (ADR-SA-003): the facts on disk plus the new
        // keys this batch has already staged for the section.
        let mut section_sizes: BTreeMap<SectionKind, usize> = BTreeMap::new();
        for observation in observations {
            let staged_fact = staged
                .get(&(observation.section, observation.key.clone()))
                .map(|(source, seen)| ProfileFact {
                    value: String::new(),
                    source: source.clone(),
                    updated_at: *seen,
                });
            let existing =
                staged_fact.or_else(|| self.fact(observation.section, &observation.key).cloned());
            let section_size = {
                let size = section_sizes
                    .entry(observation.section)
                    .or_insert_with(|| self.section(observation.section).len());
                *size
            };
            let incoming_source = observation.kind.change_source(observation.confidence);
            let decision = match self.evaluate_observation(observation) {
                // A full section refuses NEW facts until one is retired;
                // overriding an existing key replaces in place and never
                // grows the section (ADR-SA-003 decision 4).
                RecordDecision::Record
                    if existing.is_none() && section_size >= MAX_FACTS_PER_SECTION =>
                {
                    RecordDecision::Ignore {
                        reason: IgnoreReason::CapacityFull,
                    }
                }
                RecordDecision::Record if patches.len() >= budget => RecordDecision::NeedsReview,
                RecordDecision::Record
                    if existing.as_ref().is_some_and(|fact| {
                        fact.supersede(&incoming_source, at) == SupersedeDecision::Keep
                    }) =>
                {
                    RecordDecision::Ignore {
                        reason: IgnoreReason::Superseded,
                    }
                }
                other => other,
            };
            if let RecordDecision::Record = decision {
                if let Some(fact) = &existing {
                    overrides.push(format!(
                        "{}: {:?} ({}) superseded by {:?} ({})",
                        observation.key,
                        fact.value,
                        fact.source.kind(),
                        observation.value,
                        incoming_source.kind()
                    ));
                }
                staged.insert(
                    (observation.section, observation.key.clone()),
                    (incoming_source.clone(), at),
                );
                patches.push(FactPatch {
                    section: observation.section,
                    key: observation.key.clone(),
                    action: PatchAction::Upsert {
                        value: observation.value.clone(),
                        source: incoming_source,
                    },
                });
                if existing.is_none() {
                    section_sizes.insert(observation.section, section_size + 1);
                }
            }
            outcomes.push(ObservationOutcome {
                section: observation.section,
                key: observation.key.clone(),
                decision,
            });
        }
        RecordingPlan {
            patches,
            outcomes,
            overrides,
        }
    }

    pub fn fact_count(&self) -> usize {
        SectionKind::ALL
            .iter()
            .map(|kind| self.section(*kind).len())
            .sum()
    }
}

/// The outcome of planning a batch: what to commit, what to report, and
/// which existing facts were displaced (for the commit note, ADR-SA-002 §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingPlan {
    pub patches: Vec<FactPatch>,
    pub outcomes: Vec<ObservationOutcome>,
    pub overrides: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationKind {
    /// The user corrected the individual. Always recorded (I-U3b).
    Correction,
    /// A fact expected to hold across sessions.
    StableFact,
    /// A goal or project signal.
    GoalSignal,
    /// Ephemeral context; never recorded.
    Transient,
}

impl ObservationKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Correction => "correction",
            Self::StableFact => "stable_fact",
            Self::GoalSignal => "goal_signal",
            Self::Transient => "transient",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "correction" => Self::Correction,
            "stable_fact" => Self::StableFact,
            "goal_signal" => Self::GoalSignal,
            "transient" => Self::Transient,
            _ => return None,
        })
    }

    /// The provenance a recorded fact carries for this observation kind.
    /// Business rule owned by the domain: a correction is a user
    /// conversation correction; everything else is agent inference with the
    /// observation's confidence.
    pub fn change_source(self, confidence: u8) -> ChangeSource {
        match self {
            Self::Correction => ChangeSource::UserConversationCorrection,
            _ => ChangeSource::AgentInference { confidence },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub section: SectionKind,
    pub key: String,
    pub value: String,
    pub kind: ObservationKind,
    /// 0..=100; only meaningful for inference-kind observations.
    pub confidence: u8,
}

/// Minimum confidence for a non-correction observation to be recorded.
pub const MIN_CONFIDENCE: u8 = 60;

/// Soft capacity per section (ADR-SA-003 decision 4): once a section holds
/// this many facts, new facts are refused with `IgnoreReason::CapacityFull`
/// until one is retired; overriding or removing an existing key still
/// works. The config key is pending its RFC, so the default lives here as a
/// crate constant for now.
pub const MAX_FACTS_PER_SECTION: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreReason {
    EmptyValue,
    Transient,
    CredentialLike,
    LowConfidence,
    Duplicate,
    /// An existing fact outranks (or is newer than) the incoming one
    /// (I-U4 / ADR-SA-002): the user's explicit fact or correction stays.
    Superseded,
    /// The target section is at its fact capacity (ADR-SA-003): the new
    /// fact was refused; retire a fact before recording another one.
    CapacityFull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordDecision {
    Record,
    /// Over the per-turn budget; kept as an observation only (I-U5).
    NeedsReview,
    Ignore {
        reason: IgnoreReason,
    },
}

/// One observation's adjudication result, reported back to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationOutcome {
    pub section: SectionKind,
    pub key: String,
    pub decision: RecordDecision,
}

/// Heuristic red line (I-U3): credential-shaped content never enters the
/// profile, whatever the observation claims to be.
pub fn is_credential_like(key: &str, value: &str) -> bool {
    let hay = format!("{key}\n{value}").to_ascii_lowercase();
    if [
        "api_key",
        "apikey",
        "token",
        "password",
        "secret",
        "credential",
        "bearer",
    ]
    .iter()
    .any(|marker| hay.contains(marker))
    {
        return true;
    }
    if value.starts_with("sk-") || value.starts_with("ghp_") || value.starts_with("github_pat_") {
        return true;
    }
    value.len() >= 32
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-'))
}

/// One minimal fact patch (I-U4: fact-level, never a section rewrite).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactPatch {
    pub section: SectionKind,
    pub key: String,
    pub action: PatchAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchAction {
    Upsert { value: String, source: ChangeSource },
    Remove,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(kind: ObservationKind, value: &str, confidence: u8) -> Observation {
        Observation {
            section: SectionKind::Preferences,
            key: "editor".into(),
            value: value.into(),
            kind,
            confidence,
        }
    }

    #[test]
    fn corrections_are_always_recorded_but_not_credentials() {
        let profile = UserProfile::default();
        assert_eq!(
            profile.evaluate_observation(&observation(ObservationKind::Correction, "Neovim", 0)),
            RecordDecision::Record
        );
        let secret = Observation {
            key: "api_key".into(),
            ..observation(ObservationKind::Correction, "sk-abcdef123456", 100)
        };
        assert_eq!(
            profile.evaluate_observation(&secret),
            RecordDecision::Ignore {
                reason: IgnoreReason::CredentialLike
            }
        );
    }

    #[test]
    fn transient_empty_low_confidence_and_duplicates_are_ignored() {
        let profile = UserProfile::default();
        assert_eq!(
            profile.evaluate_observation(&observation(ObservationKind::Transient, "busy now", 90)),
            RecordDecision::Ignore {
                reason: IgnoreReason::Transient
            }
        );
        assert_eq!(
            profile.evaluate_observation(&observation(ObservationKind::StableFact, "   ", 90)),
            RecordDecision::Ignore {
                reason: IgnoreReason::EmptyValue
            }
        );
        assert_eq!(
            profile.evaluate_observation(&observation(ObservationKind::StableFact, "Neovim", 30)),
            RecordDecision::Ignore {
                reason: IgnoreReason::LowConfidence
            }
        );
        let existing = UserProfile {
            preferences: SectionFacts::from([(
                "editor".into(),
                ProfileFact {
                    value: "Neovim".into(),
                    source: ChangeSource::AgentInference { confidence: 70 },
                    updated_at: 1,
                },
            )]),
            ..Default::default()
        };
        assert_eq!(
            existing.evaluate_observation(&observation(ObservationKind::StableFact, "Neovim", 90)),
            RecordDecision::Ignore {
                reason: IgnoreReason::Duplicate
            }
        );
    }

    #[test]
    fn credential_heuristics_cover_prefixes_and_long_random_strings() {
        assert!(is_credential_like("token", "anything"));
        assert!(is_credential_like(
            "key",
            "ghp_1234567890abcdef1234567890abcdef1234"
        ));
        assert!(is_credential_like(
            "value",
            "aBc123-_+/==aBc123-_+/==aBc123-_+/==xyz"
        ));
        assert!(!is_credential_like("editor", "Neovim"));
        assert!(!is_credential_like("hobby", "long walks on the beach"));
    }

    #[test]
    fn plan_recording_enforces_the_budget_and_marks_overflow() {
        let profile = UserProfile::default();
        let observations: Vec<Observation> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|key| Observation {
                section: SectionKind::Preferences,
                key: (*key).into(),
                value: "value".into(),
                kind: ObservationKind::StableFact,
                confidence: 90,
            })
            .collect();
        let plan = profile.plan_recording(&observations, 3, 1_000);
        let recorded = plan
            .outcomes
            .iter()
            .filter(|o| matches!(o.decision, RecordDecision::Record))
            .count();
        let needs_review = plan
            .outcomes
            .iter()
            .filter(|o| matches!(o.decision, RecordDecision::NeedsReview))
            .count();
        assert_eq!(recorded, 3);
        assert_eq!(needs_review, 2);
        assert_eq!(plan.patches.len(), 3);
    }

    /// ADR-SA-003 decision 4: a full section refuses NEW facts with the
    /// observable capacity outcome, overriding an existing key still works
    /// (the retirement path), and retiring a fact reopens the section.
    #[test]
    fn plan_recording_refuses_new_facts_while_the_section_is_full() {
        let mut profile = UserProfile::default();
        let fill: Vec<FactPatch> = (0..MAX_FACTS_PER_SECTION)
            .map(|index| FactPatch {
                section: SectionKind::Preferences,
                key: format!("fact-{index}"),
                action: PatchAction::Upsert {
                    value: "value".into(),
                    source: ChangeSource::AgentInference { confidence: 80 },
                },
            })
            .collect();
        profile.apply_patches(&fill, 1_000);

        let new_fact = Observation {
            section: SectionKind::Preferences,
            key: "editor".into(),
            value: "Neovim".into(),
            kind: ObservationKind::StableFact,
            confidence: 90,
        };
        let plan = profile.plan_recording(std::slice::from_ref(&new_fact), 3, 2_000);
        assert_eq!(
            plan.patches,
            Vec::new(),
            "a full section must refuse new facts"
        );
        assert_eq!(
            plan.outcomes[0].decision,
            RecordDecision::Ignore {
                reason: IgnoreReason::CapacityFull
            }
        );

        // Overriding an existing key replaces in place: capacity does not
        // block it.
        let in_place = Observation {
            key: "fact-0".into(),
            value: "updated".into(),
            ..new_fact.clone()
        };
        let plan = profile.plan_recording(&[in_place], 3, 2_000);
        assert_eq!(plan.patches.len(), 1, "an override is not a new fact");
        assert!(matches!(plan.outcomes[0].decision, RecordDecision::Record));

        // Retiring one fact reopens the section for a new key.
        profile.apply_patches(
            &[FactPatch {
                section: SectionKind::Preferences,
                key: "fact-1".into(),
                action: PatchAction::Remove,
            }],
            3_000,
        );
        let plan = profile.plan_recording(&[new_fact], 3, 4_000);
        assert_eq!(plan.patches.len(), 1, "the retired slot is reusable");
        assert!(matches!(plan.outcomes[0].decision, RecordDecision::Record));
    }

    /// The capacity check accounts for facts the same batch has already
    /// staged: a section with one slot left takes the first new fact and
    /// refuses the second.
    #[test]
    fn plan_recording_capacity_accounts_for_staged_batch_facts() {
        let mut profile = UserProfile::default();
        let fill: Vec<FactPatch> = (0..MAX_FACTS_PER_SECTION - 1)
            .map(|index| FactPatch {
                section: SectionKind::Preferences,
                key: format!("fact-{index}"),
                action: PatchAction::Upsert {
                    value: "value".into(),
                    source: ChangeSource::AgentInference { confidence: 80 },
                },
            })
            .collect();
        profile.apply_patches(&fill, 1_000);

        let observations: Vec<Observation> = ["a", "b"]
            .iter()
            .map(|key| Observation {
                section: SectionKind::Preferences,
                key: (*key).into(),
                value: "value".into(),
                kind: ObservationKind::StableFact,
                confidence: 90,
            })
            .collect();
        let plan = profile.plan_recording(&observations, 3, 2_000);
        assert!(matches!(plan.outcomes[0].decision, RecordDecision::Record));
        assert_eq!(
            plan.outcomes[1].decision,
            RecordDecision::Ignore {
                reason: IgnoreReason::CapacityFull
            }
        );
        assert_eq!(plan.patches.len(), 1);
    }

    #[test]
    fn observation_kind_maps_its_own_provenance() {
        assert_eq!(
            ObservationKind::Correction.change_source(0),
            ChangeSource::UserConversationCorrection
        );
        assert_eq!(
            ObservationKind::StableFact.change_source(80),
            ChangeSource::AgentInference { confidence: 80 }
        );
    }

    fn fact(source: ChangeSource, updated_at: TimestampMs) -> ProfileFact {
        ProfileFact {
            value: "existing".into(),
            source,
            updated_at,
        }
    }

    /// ADR-SA-002 conflict matrix: 3 sources × old/new × same priority.
    /// The red line: a high-confidence inference NEVER displaces a user's
    /// explicit fact or correction.
    #[test]
    fn supersede_conflict_matrix() {
        let old = 1_000;
        let new = 2_000;
        let explicit = fact(ChangeSource::UserExplicitEdit, old);
        let correction = fact(ChangeSource::UserConversationCorrection, old);
        let inference = fact(ChangeSource::AgentInference { confidence: 95 }, old);

        // Lower priority incoming never overrides, whatever its confidence.
        for existing in [&explicit, &correction] {
            assert_eq!(
                existing.supersede(&ChangeSource::AgentInference { confidence: 99 }, new),
                SupersedeDecision::Keep,
                "inference must not override {:?}",
                existing.source
            );
        }
        // A conversation correction is lower priority than an explicit edit,
        // so it must not displace the user's own edit even when newer.
        assert_eq!(
            explicit.supersede(&ChangeSource::UserConversationCorrection, new),
            SupersedeDecision::Keep
        );
        assert_eq!(
            correction.supersede(&ChangeSource::UserExplicitEdit, new),
            SupersedeDecision::Override
        );
        assert_eq!(
            correction.supersede(&ChangeSource::AgentInference { confidence: 95 }, new),
            SupersedeDecision::Keep
        );
        // Same priority: newer wins, older loses.
        assert_eq!(
            inference.supersede(&ChangeSource::AgentInference { confidence: 10 }, new),
            SupersedeDecision::Override
        );
        assert_eq!(
            fact(ChangeSource::AgentInference { confidence: 10 }, new)
                .supersede(&ChangeSource::AgentInference { confidence: 95 }, old),
            SupersedeDecision::Keep
        );
        // Higher priority incoming overrides an older lower fact.
        assert_eq!(
            inference.supersede(&ChangeSource::UserExplicitEdit, new),
            SupersedeDecision::Override
        );
        // Simultaneous same-priority: incoming wins (>=).
        assert_eq!(
            fact(ChangeSource::UserExplicitEdit, new)
                .supersede(&ChangeSource::UserExplicitEdit, new),
            SupersedeDecision::Override
        );
    }

    /// The red line end-to-end: a high-confidence inference observation on a
    /// key the user explicitly edited produces `Superseded`, not a write.
    #[test]
    fn plan_recording_never_lets_inference_override_user_edits() {
        let mut profile = UserProfile::default();
        profile.apply_patches(
            &[FactPatch {
                section: SectionKind::Preferences,
                key: "editor".into(),
                action: PatchAction::Upsert {
                    value: "Helix".into(),
                    source: ChangeSource::UserExplicitEdit,
                },
            }],
            1_000,
        );
        let plan = profile.plan_recording(
            &[observation(ObservationKind::StableFact, "Neovim", 99)],
            3,
            2_000,
        );
        assert_eq!(plan.patches, Vec::new(), "no patch may be planned");
        assert!(matches!(
            plan.outcomes[0].decision,
            RecordDecision::Ignore {
                reason: IgnoreReason::Superseded
            }
        ));
        // A user correction on a key the agent previously inferred DOES
        // override the inference (rank 2 > rank 1), with the displaced fact
        // recorded in the plan's overrides for the commit note.
        let mut profile = UserProfile::default();
        profile.apply_patches(
            &[FactPatch {
                section: SectionKind::Preferences,
                key: "editor".into(),
                action: PatchAction::Upsert {
                    value: "VS Code".into(),
                    source: ChangeSource::AgentInference { confidence: 90 },
                },
            }],
            1_000,
        );
        let plan = profile.plan_recording(
            &[observation(ObservationKind::Correction, "Neovim", 0)],
            3,
            2_000,
        );
        assert_eq!(plan.patches.len(), 1);
        assert_eq!(plan.overrides.len(), 1);
        assert!(plan.overrides[0].contains("agent-inference"));
    }

    #[test]
    fn section_vocabulary_round_trips() {
        for kind in SectionKind::ALL {
            assert_eq!(SectionKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(SectionKind::parse("nonsense"), None);
    }

    #[test]
    fn change_source_vocabulary_round_trips_with_confidence() {
        for source in [
            ChangeSource::UserExplicitEdit,
            ChangeSource::UserConversationCorrection,
            ChangeSource::AgentInference { confidence: 70 },
        ] {
            let restored =
                ChangeSource::from_parts(source.kind(), source.confidence()).expect("valid");
            assert_eq!(restored, source);
        }
        assert!(ChangeSource::from_parts("nonsense", None).is_err());
        assert!(ChangeSource::from_parts("agent-inference", None).is_err());
        assert!(ChangeSource::from_parts("user-explicit-edit", Some(10)).is_err());
    }

    #[test]
    fn patches_upsert_and_remove_and_notes_survive() {
        let mut profile = UserProfile {
            notes: "user's own notes".into(),
            ..Default::default()
        };
        let at = 5;
        profile.apply_patches(
            &[FactPatch {
                section: SectionKind::Preferences,
                key: "editor".into(),
                action: PatchAction::Upsert {
                    value: "Neovim".into(),
                    source: ChangeSource::UserConversationCorrection,
                },
            }],
            at,
        );
        assert_eq!(
            profile
                .fact(SectionKind::Preferences, "editor")
                .expect("present")
                .value,
            "Neovim"
        );
        profile.apply_patches(
            &[FactPatch {
                section: SectionKind::Preferences,
                key: "editor".into(),
                action: PatchAction::Remove,
            }],
            at,
        );
        assert!(profile.fact(SectionKind::Preferences, "editor").is_none());
        assert_eq!(profile.notes, "user's own notes");
        assert_eq!(profile.fact_count(), 0);
    }
}
