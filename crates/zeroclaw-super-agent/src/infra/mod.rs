//! Infrastructure: document stores, negotiation log, clock, and the
//! isolation-guarded default paths. All persistence maps pure domain types
//! through infra-owned serialization DTOs; domain never carries serde.

pub mod clock;
pub mod doc_store;
pub mod document;
pub mod negotiation_log_jsonl;
pub mod paths;
pub mod profile_memory;
pub mod profile_store;
pub mod self_store;

use anyhow::Result;
use std::path::Path;
use std::sync::Arc;

use crate::application::ports::{
    Clock, NegotiationEventLog, ProfileDocumentStore, SelfDocumentStore,
};
use crate::domain::profile::UserProfile;
use crate::domain::selfhood::SelfState;
use crate::infra::doc_store::{ExternalEditPolicy, MdDocumentStore};

/// The Self document store: external edits are restored, never absorbed
/// (invariant I-S5).
pub fn markdown_self_store(root: &Path) -> Result<Arc<dyn SelfDocumentStore>> {
    Ok(Arc::new(MdDocumentStore::<SelfState>::new(
        root,
        ExternalEditPolicy::Restore,
    )?))
}

/// The UserProfile document store: external user edits are absorbed as
/// `UserExplicitEdit` revisions (invariant I-U2).
pub fn markdown_profile_store(root: &Path) -> Result<Arc<dyn ProfileDocumentStore>> {
    Ok(Arc::new(MdDocumentStore::<UserProfile>::new(
        root,
        ExternalEditPolicy::Absorb,
    )?))
}

/// The negotiation event log (JSONL, append-only).
pub fn jsonl_negotiation_log(root: &Path) -> Result<Arc<dyn NegotiationEventLog>> {
    Ok(Arc::new(
        crate::infra::negotiation_log_jsonl::JsonlNegotiationLog::new(root)?,
    ))
}

/// The wall-clock implementation (UTC dates in M1).
pub fn system_clock() -> Arc<dyn Clock> {
    Arc::new(clock::SystemClock)
}
