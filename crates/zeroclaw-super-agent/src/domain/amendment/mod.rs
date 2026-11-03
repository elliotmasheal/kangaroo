//! SelfAmendment: the only domain operation that changes Earned traits,
//! behind an `AmendmentAuthority`. One operation, two authority
//! implementations — the operation is always legal; who may trigger it is
//! the only variable (design doc §3.3.1).

use crate::domain::ChangeKind;
use crate::domain::selfhood::EarnedPatch;

/// Evidence that an amendment came from experience (I-S2). Free-form notes
/// for M1; structured event references arrive with the Experience wiring in
/// M2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExperienceRef {
    pub note: String,
}

/// Amendment batch budget: one amendment may carry at most this many patches.
pub const MAX_PATCHES_PER_AMENDMENT: usize = 8;

/// An Earned-only amendment request. Type-level guarantee: no field can
/// express a change to `GivenFacts` — Given changes go through negotiation
/// (§3.3.2), never through `SelfAmendment`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmendmentRequest {
    pub patches: Vec<EarnedPatch>,
    pub evidence: Vec<ExperienceRef>,
}

impl AmendmentRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.patches.is_empty() {
            return Err("an amendment must carry at least one patch".into());
        }
        if self.patches.len() > MAX_PATCHES_PER_AMENDMENT {
            return Err(format!(
                "an amendment carries at most {MAX_PATCHES_PER_AMENDMENT} patches"
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorityDecision {
    Allow,
    Deny { reason: String },
}

/// Who may trigger a `SelfAmendment`. Production builds wire only
/// [`ExperienceAuthority`]; [`DeveloperAuthority`] exists solely in builds
/// compiled with the `dev-self-amendment` feature (double gate: compile-time
/// cfg + explicit runtime switch, default off).
pub trait AmendmentAuthority: Send + Sync {
    fn authorize(&self, request: &AmendmentRequest) -> AuthorityDecision;

    /// Which audit vocabulary commits under this authority carry.
    fn change_kind(&self) -> ChangeKind;
}

/// Production authority: experience-driven amendments only. The request must
/// carry at least one experience reference — an evidence-free Earned change
/// has no provenance and is denied.
pub struct ExperienceAuthority;

impl AmendmentAuthority for ExperienceAuthority {
    fn authorize(&self, request: &AmendmentRequest) -> AuthorityDecision {
        if request.evidence.is_empty() {
            return AuthorityDecision::Deny {
                reason: "an experience-driven amendment requires at least one experience reference"
                    .into(),
            };
        }
        AuthorityDecision::Allow
    }

    fn change_kind(&self) -> ChangeKind {
        ChangeKind::Amendment
    }
}

// Compile-time gate: DeveloperAuthority is ABSENT from release builds — cfg
// gating, never #[allow(dead_code)]. In a debug build the authority is still
// inert unless the explicit runtime switch was set to on at construction.
#[cfg(feature = "dev-self-amendment")]
pub struct DeveloperAuthority {
    enabled: bool,
}

#[cfg(feature = "dev-self-amendment")]
impl DeveloperAuthority {
    /// The runtime switch. Default is off; only an explicit `true` enables
    /// the developer amendment path in a debug build.
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

#[cfg(feature = "dev-self-amendment")]
impl AmendmentAuthority for DeveloperAuthority {
    fn authorize(&self, _request: &AmendmentRequest) -> AuthorityDecision {
        if self.enabled {
            AuthorityDecision::Allow
        } else {
            AuthorityDecision::Deny {
                reason: "the dev self-amendment runtime switch is off (default)".into(),
            }
        }
    }

    fn change_kind(&self) -> ChangeKind {
        ChangeKind::DeveloperAmendment
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::selfhood::EarnedPatch;

    fn set_trait() -> EarnedPatch {
        EarnedPatch::SetTrait {
            kind: "patience".into(),
            strength: 40,
        }
    }

    #[test]
    fn experience_authority_requires_evidence() {
        let authority = ExperienceAuthority;
        let without = AmendmentRequest {
            patches: vec![set_trait()],
            evidence: vec![],
        };
        assert!(matches!(
            authority.authorize(&without),
            AuthorityDecision::Deny { .. }
        ));
        let with = AmendmentRequest {
            patches: vec![set_trait()],
            evidence: vec![ExperienceRef {
                note: "user thanked patient explanations three times".into(),
            }],
        };
        assert_eq!(authority.authorize(&with), AuthorityDecision::Allow);
        assert_eq!(authority.change_kind(), ChangeKind::Amendment);
    }

    #[test]
    fn amendment_validation_enforces_batch_budget() {
        let empty = AmendmentRequest {
            patches: vec![],
            evidence: vec![],
        };
        assert!(empty.validate().is_err());
        let patches = vec![set_trait(); MAX_PATCHES_PER_AMENDMENT + 1];
        let over = AmendmentRequest {
            patches,
            evidence: vec![],
        };
        assert!(over.validate().is_err());
        let single = AmendmentRequest {
            patches: vec![set_trait()],
            evidence: vec![],
        };
        assert!(single.validate().is_ok());
    }

    #[cfg(feature = "dev-self-amendment")]
    #[test]
    fn developer_authority_respects_the_runtime_switch() {
        let off = DeveloperAuthority::new(false);
        let request = AmendmentRequest {
            patches: vec![set_trait()],
            evidence: vec![],
        };
        assert!(matches!(
            off.authorize(&request),
            AuthorityDecision::Deny { .. }
        ));
        let on = DeveloperAuthority::new(true);
        assert_eq!(on.authorize(&request), AuthorityDecision::Allow);
        assert_eq!(on.change_kind(), ChangeKind::DeveloperAmendment);
    }
}
