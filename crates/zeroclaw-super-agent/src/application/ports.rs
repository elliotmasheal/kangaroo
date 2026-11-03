//! Ports the application layer needs from the outside world. `infra`
//! implements them; tests implement in-memory variants.

use crate::domain::profile::UserProfile;
use crate::domain::selfhood::SelfState;
use crate::domain::selfhood::negotiation::GivenAmendmentEvent;
use crate::domain::{ChangeKind, DateYmd, Fingerprint, RevisionId, TimestampMs};
use anyhow::Result;

/// What a successful write produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitReceipt {
    pub revision: RevisionId,
    pub fingerprint: Fingerprint,
}

/// How a detected external edit was handled at load time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalEditHandling {
    /// UserProfile: the user's hand edit was parsed and absorbed as a
    /// `UserExplicitEdit` revision (invariant I-U2 — user edits outrank).
    AbsorbedAsUserEdit,
    /// Self: the edit was rejected and the document restored from the last
    /// revision (invariant I-S5 — Self is not user-editable).
    RestoredFromRevision,
}

/// One document load result. A fresh load is mandatory before every write.
#[derive(Debug, Clone)]
pub struct LoadedDoc<S> {
    /// `None` before initialization.
    pub state: Option<S>,
    /// Fingerprint of the on-disk rendering; `Fingerprint(0)` when absent.
    pub fingerprint: Fingerprint,
    pub external_edit: Option<ExternalEditHandling>,
}

/// Store port for the Self document. Single writer per process: all
/// operations serialize inside the store (invariant I-S3).
pub trait SelfDocumentStore: Send + Sync {
    fn load(&self) -> Result<LoadedDoc<SelfState>>;
    fn commit(
        &self,
        expected: Fingerprint,
        state: &SelfState,
        kind: ChangeKind,
        note: &str,
        at: TimestampMs,
    ) -> Result<CommitReceipt>;
    fn rollback(
        &self,
        expected: Fingerprint,
        to: RevisionId,
        at: TimestampMs,
    ) -> Result<CommitReceipt>;
}

/// Store port for the UserProfile document (invariants I-U1/I-U2).
pub trait ProfileDocumentStore: Send + Sync {
    fn load(&self) -> Result<LoadedDoc<UserProfile>>;
    fn commit(
        &self,
        expected: Fingerprint,
        state: &UserProfile,
        kind: ChangeKind,
        note: &str,
        at: TimestampMs,
    ) -> Result<CommitReceipt>;
    fn rollback(
        &self,
        expected: Fingerprint,
        to: RevisionId,
        at: TimestampMs,
    ) -> Result<CommitReceipt>;
}

/// Append-only event log for Given negotiation (I-N4). Fold helpers live in
/// `domain::selfhood::negotiation`.
pub trait NegotiationEventLog: Send + Sync {
    fn append(&self, event: &GivenAmendmentEvent) -> Result<()>;
    fn load_all(&self) -> Result<Vec<GivenAmendmentEvent>>;
}

/// Time source. Kept a port so tests pin dates.
pub trait Clock: Send + Sync {
    fn today(&self) -> DateYmd;
    fn now_ms(&self) -> TimestampMs;
}
