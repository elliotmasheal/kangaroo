//! Self use cases: identity initialization (once, audited), projection for
//! turn context, `SelfAmendment` behind an authority, and Given negotiation.

use anyhow::{Result, bail};
use std::sync::Arc;
use zeroclaw_log::{Action, Event, record};

use crate::application::ports::{Clock, LoadedDoc, NegotiationEventLog, SelfDocumentStore};
use crate::domain::amendment::{AmendmentAuthority, AmendmentRequest, AuthorityDecision};
use crate::domain::selfhood::negotiation::{
    GivenAmendmentEvent, GivenAmendmentRequest, GivenDecision, GivenField, GivenValue,
    NegotiationPolicy, RequestId, fold_requests, open_request,
};
use crate::domain::selfhood::{
    Gender, GivenFacts, SelfState, completed_years, existence_days, is_anniversary,
};
use crate::domain::{ChangeKind, DateYmd, RevisionId};

/// Input for the identity initialization use case (§3.4): runs once, is
/// validated per field, and is audited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitializationInput {
    pub name: String,
    pub birthday: DateYmd,
    pub gender: Gender,
    pub initial_style: String,
}

impl InitializationInput {
    fn into_given(self) -> GivenFacts {
        GivenFacts {
            name: self.name.trim().to_owned(),
            birthday: self.birthday,
            gender: self.gender,
            initial_style: self.initial_style.trim().to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfProjection {
    pub initialized: bool,
    /// Markdown narrative for the turn context; includes time-anchor lines
    /// (existence length, anniversary) computed against the clock (I-S4).
    pub narrative: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmendmentOutcome {
    pub revision: RevisionId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GivenOutcome {
    pub request_id: RequestId,
    pub accepted: bool,
    pub reason: String,
    pub revision: Option<RevisionId>,
}

/// Self use cases. One service per document store keeps the two subjects'
/// write paths independent (I-X2).
pub struct SelfService {
    store: Arc<dyn SelfDocumentStore>,
    negotiation_log: Arc<dyn NegotiationEventLog>,
    clock: Arc<dyn Clock>,
    policy: Arc<dyn NegotiationPolicy>,
}

impl SelfService {
    pub fn new(
        store: Arc<dyn SelfDocumentStore>,
        negotiation_log: Arc<dyn NegotiationEventLog>,
        clock: Arc<dyn Clock>,
        policy: Arc<dyn NegotiationPolicy>,
    ) -> Self {
        Self {
            store,
            negotiation_log,
            clock,
            policy,
        }
    }

    /// Identity initialization (§3.4): validated, once only, audited.
    pub fn initialize(&self, input: InitializationInput) -> Result<RevisionId> {
        let loaded = self.store.load()?;
        if loaded.state.is_some() {
            bail!("this individual is already initialized; initialization runs once");
        }
        let today = self.clock.today();
        let given = input.into_given();
        given
            .validate(today)
            .map_err(|reason| anyhow::Error::msg(format!("initialization rejected: {reason}")))?;
        let state = SelfState::initialize(given);
        let receipt = self.store.commit(
            loaded.fingerprint,
            &state,
            ChangeKind::Initialize,
            "identity initialization",
            self.clock.now_ms(),
        )?;
        record!(
            INFO,
            Event::new(module_path!(), Action::Register).with_attrs(serde_json::json!({
                "revision": receipt.revision.0,
            })),
            "identity initialized"
        );
        Ok(receipt.revision)
    }

    /// Fresh load; surfaces external-edit handling to the caller.
    pub fn current(&self) -> Result<LoadedDoc<SelfState>> {
        self.store.load()
    }

    /// Per-turn self projection: reads the document now, no caching.
    pub fn projection(&self) -> Result<SelfProjection> {
        let loaded = self.store.load()?;
        let Some(state) = loaded.state else {
            return Ok(SelfProjection {
                initialized: false,
                narrative: String::new(),
            });
        };
        let today = self.clock.today();
        let birthday = state.given.birthday;
        let mut narrative = state.render_narrative();
        narrative.push_str(&format!(
            "\n- existence: {} days ({} full years)\n",
            existence_days(birthday, today),
            completed_years(birthday, today),
        ));
        if is_anniversary(birthday, today) {
            narrative.push_str("- today is this individual's birthday\n");
        }
        Ok(SelfProjection {
            initialized: true,
            narrative,
        })
    }

    /// Submits a `SelfAmendment` through the given authority. The authority
    /// decides; a denial is final and logged, never silently retried.
    pub fn submit_amendment(
        &self,
        request: AmendmentRequest,
        authority: &dyn AmendmentAuthority,
    ) -> Result<AmendmentOutcome> {
        if let AuthorityDecision::Deny { reason } = authority.authorize(&request) {
            record!(
                WARN,
                Event::new(module_path!(), Action::Reject).with_attrs(serde_json::json!({
                    "reason": reason,
                })),
                "self amendment denied by authority"
            );
            bail!("self amendment denied by authority: {reason}");
        }
        request
            .validate()
            .map_err(|reason| anyhow::Error::msg(format!("invalid amendment: {reason}")))?;
        let loaded = self.store.load()?;
        let Some(mut state) = loaded.state else {
            bail!("this individual is not initialized yet");
        };
        state
            .apply_earned_patches(&request.patches)
            .map_err(anyhow::Error::msg)?;
        let receipt = self.store.commit(
            loaded.fingerprint,
            &state,
            authority.change_kind(),
            "self amendment",
            self.clock.now_ms(),
        )?;
        Ok(AmendmentOutcome {
            revision: receipt.revision,
        })
    }

    /// Proposes a Given change: persists the proposal, lets the policy rule,
    /// persists the decision with its reason, and only an acceptance writes
    /// the document (I-N1..I-N4).
    pub fn propose_given_change(
        &self,
        field: GivenField,
        value: GivenValue,
        reason_from_user: impl Into<String>,
    ) -> Result<GivenOutcome> {
        let loaded = self.store.load()?;
        let Some(state) = loaded.state else {
            bail!("this individual is not initialized yet");
        };
        let events = self.negotiation_log.load_all()?;
        if let Some(open) = open_request(&events) {
            bail!(
                "a Given amendment request is already pending (request {}); wait for a decision or withdraw it",
                open.id.0
            );
        }
        let id = RequestId(events.len() as u64 + 1);
        let request = GivenAmendmentRequest::proposed(
            id,
            field,
            value,
            reason_from_user.into(),
            self.clock.now_ms(),
        );
        self.negotiation_log
            .append(&GivenAmendmentEvent::Proposed {
                request: request.clone(),
            })?;
        let decision = self
            .policy
            .evaluate(&request, &state.given, self.clock.today());
        match decision {
            GivenDecision::Accept { reason } => {
                let mut candidate = state.clone();
                // The policy applied the same change to its own copy when it
                // accepted, so applying here cannot fail.
                candidate
                    .given
                    .apply_field(request.field, request.proposed.clone())
                    .map_err(anyhow::Error::msg)?;
                match self.store.commit(
                    loaded.fingerprint,
                    &candidate,
                    ChangeKind::Negotiated,
                    "given fact accepted through negotiation",
                    self.clock.now_ms(),
                ) {
                    Ok(receipt) => {
                        self.negotiation_log.append(&GivenAmendmentEvent::Decided {
                            request_id: id,
                            accepted: true,
                            reason: reason.clone(),
                            at: self.clock.now_ms(),
                        })?;
                        record!(
                            INFO,
                            Event::new(module_path!(), Action::Approve)
                                .with_attrs(serde_json::json!({ "request_id": id.0 }),),
                            "given amendment accepted through negotiation"
                        );
                        Ok(GivenOutcome {
                            request_id: id,
                            accepted: true,
                            reason,
                            revision: Some(receipt.revision),
                        })
                    }
                    Err(error) => {
                        let reason = format!("the individual could not apply the change: {error}");
                        self.negotiation_log.append(&GivenAmendmentEvent::Decided {
                            request_id: id,
                            accepted: false,
                            reason: reason.clone(),
                            at: self.clock.now_ms(),
                        })?;
                        Ok(GivenOutcome {
                            request_id: id,
                            accepted: false,
                            reason,
                            revision: None,
                        })
                    }
                }
            }
            GivenDecision::Reject { reason } => {
                self.negotiation_log.append(&GivenAmendmentEvent::Decided {
                    request_id: id,
                    accepted: false,
                    reason: reason.clone(),
                    at: self.clock.now_ms(),
                })?;
                record!(
                    INFO,
                    Event::new(module_path!(), Action::Reject).with_attrs(serde_json::json!({
                        "request_id": id.0,
                    })),
                    "given amendment rejected through negotiation"
                );
                Ok(GivenOutcome {
                    request_id: id,
                    accepted: false,
                    reason,
                    revision: None,
                })
            }
        }
    }

    /// Withdraws an open request. Returns `false` when the request is not
    /// open; withdrawal never changes the document (I-N3).
    pub fn withdraw_given_request(&self, request_id: RequestId) -> Result<bool> {
        let events = self.negotiation_log.load_all()?;
        let is_open = fold_requests(&events)
            .into_iter()
            .any(|r| r.id == request_id && r.is_open());
        if !is_open {
            return Ok(false);
        }
        self.negotiation_log
            .append(&GivenAmendmentEvent::Withdrawn {
                request_id,
                at: self.clock.now_ms(),
            })?;
        Ok(true)
    }

    pub fn pending_given_request(&self) -> Result<Option<RequestId>> {
        let events = self.negotiation_log.load_all()?;
        Ok(open_request(&events).map(|r| r.id))
    }
}
