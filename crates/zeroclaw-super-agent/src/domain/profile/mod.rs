//! The UserProfile aggregate: structured facts about the user. This is the
//! only document a user may hand-edit (decision: asymmetric edit rights);
//! the Self document is restored, never absorbed.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::domain::TimestampMs;

/// The five fixed profile sections. A fixed set keeps machine merging and
/// user edits diffable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SectionKind {
    Identity,
    Environment,
    Preferences,
    Goals,
    Corrections,
}

/// Facts within one section, keyed by a free-form key.
pub type SectionFacts = BTreeMap<String, ProfileFact>;

/// Where a fact came from; drives the override priority (invariant I-U4:
/// user explicit edits outrank conversation corrections outrank inference).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeSource {
    UserExplicitEdit,
    UserConversationCorrection,
    AgentInference { confidence: u8 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileFact {
    pub value: String,
    pub source: ChangeSource,
    pub updated_at: TimestampMs,
}

/// The UserProfile aggregate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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

    pub fn apply_patch(&mut self, patch: &FactPatch, at: TimestampMs) {
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

    pub fn fact_count(&self) -> usize {
        self.section(SectionKind::Identity).len()
            + self.section(SectionKind::Environment).len()
            + self.section(SectionKind::Preferences).len()
            + self.section(SectionKind::Goals).len()
            + self.section(SectionKind::Corrections).len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IgnoreReason {
    EmptyValue,
    Transient,
    CredentialLike,
    LowConfidence,
    Duplicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordDecision {
    Record,
    /// Over the per-turn budget; kept as an observation only (I-U5).
    NeedsReview,
    Ignore {
        reason: IgnoreReason,
    },
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

/// The update criterion (invariant I-U3): what deserves to be written.
pub struct WorthRecording;

impl WorthRecording {
    pub fn decide(
        &self,
        observation: &Observation,
        existing: Option<&ProfileFact>,
    ) -> RecordDecision {
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
        if existing.is_some_and(|fact| fact.value == observation.value) {
            return RecordDecision::Ignore {
                reason: IgnoreReason::Duplicate,
            };
        }
        RecordDecision::Record
    }
}

/// One minimal fact patch (I-U4: fact-level, never a section rewrite).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactPatch {
    pub section: SectionKind,
    pub key: String,
    pub action: PatchAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
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
        let policy = WorthRecording;
        assert_eq!(
            policy.decide(&observation(ObservationKind::Correction, "Neovim", 0), None),
            RecordDecision::Record
        );
        let secret = Observation {
            key: "api_key".into(),
            value: "sk-abcdef123456".into(),
            ..observation(ObservationKind::Correction, "sk-abcdef123456", 100)
        };
        assert_eq!(
            policy.decide(&secret, None),
            RecordDecision::Ignore {
                reason: IgnoreReason::CredentialLike
            }
        );
    }

    #[test]
    fn transient_empty_low_confidence_and_duplicates_are_ignored() {
        let policy = WorthRecording;
        assert_eq!(
            policy.decide(
                &observation(ObservationKind::Transient, "busy now", 90),
                None
            ),
            RecordDecision::Ignore {
                reason: IgnoreReason::Transient
            }
        );
        assert_eq!(
            policy.decide(&observation(ObservationKind::StableFact, "   ", 90), None),
            RecordDecision::Ignore {
                reason: IgnoreReason::EmptyValue
            }
        );
        assert_eq!(
            policy.decide(
                &observation(ObservationKind::StableFact, "Neovim", 30),
                None
            ),
            RecordDecision::Ignore {
                reason: IgnoreReason::LowConfidence
            }
        );
        let existing = ProfileFact {
            value: "Neovim".into(),
            source: ChangeSource::AgentInference { confidence: 70 },
            updated_at: 1,
        };
        assert_eq!(
            policy.decide(
                &observation(ObservationKind::StableFact, "Neovim", 90),
                Some(&existing)
            ),
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
    fn patches_upsert_and_remove_and_notes_survive() {
        let mut profile = UserProfile {
            notes: "user's own notes".into(),
            ..Default::default()
        };
        let at = 5;
        profile.apply_patch(
            &FactPatch {
                section: SectionKind::Preferences,
                key: "editor".into(),
                action: PatchAction::Upsert {
                    value: "Neovim".into(),
                    source: ChangeSource::UserConversationCorrection,
                },
            },
            at,
        );
        assert_eq!(
            profile
                .fact(SectionKind::Preferences, "editor")
                .expect("present")
                .value,
            "Neovim"
        );
        profile.apply_patch(
            &FactPatch {
                section: SectionKind::Preferences,
                key: "editor".into(),
                action: PatchAction::Remove,
            },
            at,
        );
        assert!(profile.fact(SectionKind::Preferences, "editor").is_none());
        assert_eq!(profile.notes, "user's own notes");
        assert_eq!(profile.fact_count(), 0);
    }
}
