//! The stable identity-initialization contract (adapter-agnostic). An outer
//! interaction adapter (an intake flow today, something else tomorrow)
//! assembles an [`InitializationDraft`]; the core validates it and commits
//! once, atomically. The core never sees questions, question ids, progress,
//! not-disclosed sentinels, wording, or sessions — those belong to the
//! adapter (dependency direction guarded by `tests/core_boundary.rs`).

use crate::domain::profile::Observation;
use crate::domain::selfhood::GivenFacts;

/// Everything the identity initialization needs, as produced by an outer
/// interaction adapter. `given` may carry explicit not-disclosed markers the
/// adapter chose — validity is still enforced in the core use case
/// ([`SelfService::initialize_from_draft`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitializationDraft {
    pub given: GivenFacts,
    /// Initial assumptions for the UserProfile, with source and confidence
    /// attached by the adapter. They flow through the profile's own
    /// recording plan, never a direct write.
    pub assumptions: Vec<Observation>,
}
