//! The Self aggregate: given facts (negotiation-only) and earned traits
//! (experience-driven). Domain term: Self; the module is `selfhood` because
//! `self` is a reserved Rust keyword.

pub mod negotiation;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::domain::{DateYmd, TimestampMs};
use negotiation::{GivenField, GivenValue};

/// Experiences required before one observation solidifies into an Earned
/// trait (invariant I-S2).
pub const SOLIDIFY_THRESHOLD: u32 = 3;
/// Strength granted when a trait first solidifies.
pub const INITIAL_TRAIT_STRENGTH: u8 = 20;
/// Strength lost per round of user negative feedback.
pub const NEGATIVE_FEEDBACK_WEAKENING: u8 = 20;
/// Upper bound for trait strength.
pub const TRAIT_STRENGTH_MAX: u8 = 100;
/// Maximum length of a name.
pub const NAME_MAX_CHARS: usize = 64;
/// Maximum length of the initial style text.
pub const STYLE_MAX_CHARS: usize = 512;
/// Earliest year a birthday may name.
pub const BIRTHDAY_EARLIEST_YEAR: i16 = 1900;

/// The gender named at initialization. More values may only arrive through a
/// schema change, not through free text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Gender {
    Female,
    Male,
    NonBinary,
    Undisclosed,
}

/// Facts given at initialization. Invariant I-S1: written once by the
/// initialization use case, afterwards only negotiable (never writable by the
/// agent, never directly editable by the user).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GivenFacts {
    pub name: String,
    pub birthday: DateYmd,
    pub gender: Gender,
    pub initial_style: String,
}

impl GivenFacts {
    /// The same validation the initialization use case applies; negotiation
    /// (I-N5) reuses it for proposed values.
    pub fn validate(&self, today: DateYmd) -> Result<(), String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("the name must not be empty".into());
        }
        if name.chars().count() > NAME_MAX_CHARS {
            return Err(format!("the name exceeds {NAME_MAX_CHARS} characters"));
        }
        if self.birthday.year < BIRTHDAY_EARLIEST_YEAR {
            return Err(format!(
                "the birthday year must be {} or later",
                BIRTHDAY_EARLIEST_YEAR
            ));
        }
        if self.birthday > today {
            return Err("the birthday must not be in the future".into());
        }
        let style = self.initial_style.trim();
        if style.is_empty() {
            return Err("the initial style must not be empty".into());
        }
        if style.chars().count() > STYLE_MAX_CHARS {
            return Err(format!(
                "the initial style exceeds {STYLE_MAX_CHARS} characters"
            ));
        }
        Ok(())
    }

    /// Applies one negotiated value. Type-level: this is the only mutation
    /// path for `given`, and it is only reachable from the negotiation
    /// accept branch.
    pub fn apply_field(&mut self, field: GivenField, value: GivenValue) -> Result<(), String> {
        match (field, value) {
            (GivenField::Name, GivenValue::Name(v)) => self.name = v,
            (GivenField::Birthday, GivenValue::Birthday(v)) => self.birthday = v,
            (GivenField::Gender, GivenValue::Gender(v)) => self.gender = v,
            (GivenField::InitialStyle, GivenValue::InitialStyle(v)) => self.initial_style = v,
            _ => return Err("the given field and the proposed value do not match".into()),
        }
        Ok(())
    }
}

/// A trait kind is a lowercase slug, so hand-written documents stay diffable.
pub fn valid_trait_kind(kind: &str) -> Result<(), String> {
    if kind.is_empty() {
        return Err("a trait kind must not be empty".into());
    }
    if kind.len() > 32 {
        return Err("a trait kind must not exceed 32 characters".into());
    }
    let mut chars = kind.chars();
    let first = chars.next().expect("non-empty checked above");
    if !first.is_ascii_lowercase() {
        return Err(format!(
            "a trait kind must start with a lowercase letter: {kind}"
        ));
    }
    if !kind
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "a trait kind may only contain lowercase letters, digits and '-': {kind}"
        ));
    }
    Ok(())
}

/// One earned trait with its current strength and evidence volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EarnedTrait {
    pub kind: String,
    pub strength: u8,
    pub evidence_count: u32,
}

/// Earned characteristics. Kept sorted by `kind` so document renders stay
/// diffable across commits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EarnedProfile {
    pub traits: Vec<EarnedTrait>,
}

/// Pending solidification counter for one trait kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationCounter {
    pub count: u32,
    pub last_seen: TimestampMs,
}

/// A patch to the Earned profile submitted through `SelfAmendment`.
/// Type-level guarantee: no variant can touch `GivenFacts` (I-S1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum EarnedPatch {
    SetTrait { kind: String, strength: u8 },
    RemoveTrait { kind: String },
}

/// Result of one experience observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExperienceOutcome {
    Counting { count: u32 },
    Solidified { kind: String },
    Reinforced { kind: String, strength: u8 },
}

/// The Self aggregate: given facts, earned traits, and pending
/// solidification counters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfState {
    pub given: GivenFacts,
    pub earned: EarnedProfile,
    pub observations: BTreeMap<String, ObservationCounter>,
}

impl SelfState {
    pub fn initialize(given: GivenFacts) -> Self {
        Self {
            given,
            earned: EarnedProfile::default(),
            observations: BTreeMap::new(),
        }
    }

    /// Records one experience observation for a trait kind. Invariant I-S2:
    /// a kind solidifies only after `SOLIDIFY_THRESHOLD` observations and
    /// then drifts slowly (+1 strength per reinforced experience).
    pub fn record_experience(
        &mut self,
        kind: &str,
        at: TimestampMs,
    ) -> Result<ExperienceOutcome, String> {
        valid_trait_kind(kind)?;
        if let Some(trait_) = self.earned.traits.iter_mut().find(|t| t.kind == kind) {
            trait_.evidence_count += 1;
            trait_.strength = trait_.strength.saturating_add(1).min(TRAIT_STRENGTH_MAX);
            return Ok(ExperienceOutcome::Reinforced {
                kind: kind.to_owned(),
                strength: trait_.strength,
            });
        }
        let counter = self
            .observations
            .entry(kind.to_owned())
            .or_insert(ObservationCounter {
                count: 0,
                last_seen: at,
            });
        counter.count += 1;
        counter.last_seen = at;
        let count = counter.count;
        if count >= SOLIDIFY_THRESHOLD {
            self.observations.remove(kind);
            self.earned.traits.push(EarnedTrait {
                kind: kind.to_owned(),
                strength: INITIAL_TRAIT_STRENGTH,
                evidence_count: count,
            });
            self.earned.traits.sort_by(|a, b| a.kind.cmp(&b.kind));
            Ok(ExperienceOutcome::Solidified {
                kind: kind.to_owned(),
            })
        } else {
            Ok(ExperienceOutcome::Counting {
                count: counter.count,
            })
        }
    }

    /// User pushback via conversation: drops any pending counter and weakens
    /// an existing trait. Returns whether a trait was weakened.
    pub fn record_negative_feedback(&mut self, kind: &str) -> bool {
        self.observations.remove(kind);
        match self.earned.traits.iter_mut().find(|t| t.kind == kind) {
            Some(trait_) => {
                trait_.strength = trait_.strength.saturating_sub(NEGATIVE_FEEDBACK_WEAKENING);
                true
            }
            None => false,
        }
    }

    /// Applies amendment patches to the Earned profile only.
    pub fn apply_earned_patches(&mut self, patches: &[EarnedPatch]) -> Result<(), String> {
        for patch in patches {
            match patch {
                EarnedPatch::SetTrait { kind, strength } => {
                    valid_trait_kind(kind)?;
                    if *strength > TRAIT_STRENGTH_MAX {
                        return Err(format!(
                            "trait strength exceeds {TRAIT_STRENGTH_MAX}: {strength}"
                        ));
                    }
                    match self.earned.traits.iter_mut().find(|t| t.kind == *kind) {
                        Some(trait_) => trait_.strength = *strength,
                        None => self.earned.traits.push(EarnedTrait {
                            kind: kind.clone(),
                            strength: *strength,
                            evidence_count: 0,
                        }),
                    }
                    self.earned.traits.sort_by(|a, b| a.kind.cmp(&b.kind));
                }
                EarnedPatch::RemoveTrait { kind } => {
                    self.earned.traits.retain(|t| t.kind != *kind);
                }
            }
        }
        Ok(())
    }

    /// Static self narrative for the document body. Time-anchor lines
    /// (existence length, anniversary) are added at projection time by the
    /// application service, which owns the clock.
    pub fn render_narrative(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("# {}\n\n", self.given.name));
        out.push_str(&format!("- birthday: {}\n", self.given.birthday));
        out.push_str(&format!("- gender: {:?}\n", self.given.gender));
        out.push_str(&format!("- initial style: {}\n", self.given.initial_style));
        if !self.earned.traits.is_empty() {
            out.push_str("\n## Earned traits\n\n");
            for trait_ in &self.earned.traits {
                out.push_str(&format!(
                    "- {} (strength {}, {} observations)\n",
                    trait_.kind, trait_.strength, trait_.evidence_count
                ));
            }
        }
        out
    }
}

/// Existence length in whole days, saturated at 0 for a future birthday.
pub fn existence_days(birthday: DateYmd, today: DateYmd) -> u64 {
    let diff = today.days_since_epoch() - birthday.days_since_epoch();
    u64::try_from(diff).unwrap_or(0)
}

/// Whole years the individual has completed.
pub fn completed_years(birthday: DateYmd, today: DateYmd) -> u32 {
    let mut years = today.year - birthday.year;
    let before_anniversary = (today.month, today.day) < (birthday.month, birthday.day);
    if before_anniversary {
        years -= 1;
    }
    u32::try_from(years).unwrap_or(0)
}

/// Whether today is the individual's birthday.
pub fn is_anniversary(birthday: DateYmd, today: DateYmd) -> bool {
    birthday.month == today.month && birthday.day == today.day
}

#[cfg(test)]
mod tests {
    use super::*;

    fn given() -> GivenFacts {
        GivenFacts {
            name: "Aria".into(),
            birthday: DateYmd::new(2026, 1, 15).expect("valid"),
            gender: Gender::Undisclosed,
            initial_style: "warm and terse".into(),
        }
    }

    #[test]
    fn validate_rejects_empty_name_future_birthday_and_long_style() {
        let today = DateYmd::new(2026, 11, 3).expect("valid");
        let mut facts = given();
        facts.name = "  ".into();
        assert!(facts.validate(today).is_err());
        facts.name = "Aria".into();
        facts.birthday = DateYmd::new(2027, 1, 1).expect("valid");
        assert!(facts.validate(today).is_err());
        facts.birthday = DateYmd::new(2026, 1, 15).expect("valid");
        facts.initial_style = "x".repeat(STYLE_MAX_CHARS + 1);
        assert!(facts.validate(today).is_err());
        facts.initial_style = "warm".into();
        assert!(facts.validate(today).is_ok());
    }

    #[test]
    fn trait_kind_rules() {
        assert!(valid_trait_kind("patience").is_ok());
        assert!(valid_trait_kind("code-review").is_ok());
        assert!(valid_trait_kind("").is_err());
        assert!(valid_trait_kind("Patience").is_err());
        assert!(valid_trait_kind("has space").is_err());
        assert!(valid_trait_kind(&"x".repeat(33)).is_err());
    }

    #[test]
    fn experience_solidifies_after_threshold_then_reinforces() {
        let mut state = SelfState::initialize(given());
        let at = 1_000;
        assert!(matches!(
            state.record_experience("patience", at).expect("ok"),
            ExperienceOutcome::Counting { count: 1 }
        ));
        assert!(matches!(
            state.record_experience("patience", at + 1).expect("ok"),
            ExperienceOutcome::Counting { count: 2 }
        ));
        assert!(matches!(
            state.record_experience("patience", at + 2).expect("ok"),
            ExperienceOutcome::Solidified { .. }
        ));
        assert!(!state.observations.contains_key("patience"));
        assert!(matches!(
            state.record_experience("patience", at + 3).expect("ok"),
            ExperienceOutcome::Reinforced { strength: 21, .. }
        ));
    }

    #[test]
    fn negative_feedback_drops_counter_and_weakens_trait() {
        let mut state = SelfState::initialize(given());
        for i in 0..SOLIDIFY_THRESHOLD {
            let _ = state.record_experience("patience", u64::from(i));
        }
        assert!(state.record_negative_feedback("patience"));
        assert_eq!(
            state
                .earned
                .traits
                .iter()
                .find(|t| t.kind == "patience")
                .expect("trait present")
                .strength,
            INITIAL_TRAIT_STRENGTH - NEGATIVE_FEEDBACK_WEAKENING
        );
        let _ = state.record_experience("patience", 100);
        assert!(!state.observations.contains_key("patience"));
    }

    #[test]
    fn earned_patches_cannot_exceed_strength_or_skip_validation() {
        let mut state = SelfState::initialize(given());
        assert!(
            state
                .apply_earned_patches(&[EarnedPatch::SetTrait {
                    kind: "patience".into(),
                    strength: TRAIT_STRENGTH_MAX + 1,
                }])
                .is_err()
        );
        assert!(
            state
                .apply_earned_patches(&[EarnedPatch::SetTrait {
                    kind: "Bad Kind".into(),
                    strength: 10,
                }])
                .is_err()
        );
        state
            .apply_earned_patches(&[
                EarnedPatch::SetTrait {
                    kind: "patience".into(),
                    strength: 40,
                },
                EarnedPatch::RemoveTrait {
                    kind: "patience".into(),
                },
            ])
            .expect("valid patches");
        assert!(state.earned.traits.iter().all(|t| t.kind != "patience"));
    }

    #[test]
    fn time_anchor_helpers() {
        let birthday = DateYmd::new(2026, 1, 15).expect("valid");
        let on_birthday = DateYmd::new(2027, 1, 15).expect("valid");
        let after = DateYmd::new(2027, 3, 1).expect("valid");
        assert!(is_anniversary(birthday, on_birthday));
        assert!(!is_anniversary(birthday, after));
        assert_eq!(completed_years(birthday, after), 1);
        assert_eq!(completed_years(birthday, on_birthday), 1);
        assert_eq!(existence_days(birthday, after), 410);
        assert_eq!(existence_days(after, birthday), 0);
    }
}
