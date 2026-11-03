//! Given-amendment negotiation (design doc §3.3.2): the only path that can
//! change `GivenFacts` after initialization. The user proposes, the
//! individual decides and may reject, and every step is recorded with a
//! reason.

use super::{Gender, GivenFacts};
use crate::domain::{DateYmd, TimestampMs};

/// Monotonic request id within one negotiation log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RequestId(pub u64);

/// Which Given fact a request targets. String forms are persistence
/// vocabulary owned by `infra`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GivenField {
    Name,
    Birthday,
    Gender,
    InitialStyle,
}

impl GivenField {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Birthday => "birthday",
            Self::Gender => "gender",
            Self::InitialStyle => "initial-style",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "name" => Self::Name,
            "birthday" => Self::Birthday,
            "gender" => Self::Gender,
            "initial-style" => Self::InitialStyle,
            _ => return None,
        })
    }
}

/// A proposed value; the variant must match the field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GivenValue {
    Name(String),
    Birthday(DateYmd),
    Gender(Gender),
    InitialStyle(String),
}

/// The individual's decision on one request. Decisions always carry a reason
/// (I-N2), including rejections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub accepted: bool,
    pub reason: String,
    pub at: TimestampMs,
}

/// One negotiation request. Status is derived: `decision == None &&
/// withdrawn_at == None` means still open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GivenAmendmentRequest {
    pub id: RequestId,
    pub field: GivenField,
    pub proposed: GivenValue,
    pub reason_from_user: String,
    pub opened_at: TimestampMs,
    pub decision: Option<Decision>,
    pub withdrawn_at: Option<TimestampMs>,
}

impl GivenAmendmentRequest {
    pub fn proposed(
        id: RequestId,
        field: GivenField,
        value: GivenValue,
        reason_from_user: String,
        opened_at: TimestampMs,
    ) -> Self {
        Self {
            id,
            field,
            proposed: value,
            reason_from_user,
            opened_at,
            decision: None,
            withdrawn_at: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.decision.is_none() && self.withdrawn_at.is_none()
    }

    pub fn status(&self) -> RequestStatus {
        if let Some(decision) = &self.decision {
            if decision.accepted {
                RequestStatus::Accepted
            } else {
                RequestStatus::Rejected
            }
        } else if self.withdrawn_at.is_some() {
            RequestStatus::Withdrawn
        } else {
            RequestStatus::Proposed
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestStatus {
    Proposed,
    Accepted,
    Rejected,
    Withdrawn,
}

/// Append-only event log vocabulary. State is the fold of these events
/// (I-N4); the log is separate from the document revision log, so a rejected
/// or withdrawn negotiation produces no document revision (I-N3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GivenAmendmentEvent {
    Proposed {
        request: GivenAmendmentRequest,
    },
    Decided {
        request_id: RequestId,
        accepted: bool,
        reason: String,
        at: TimestampMs,
    },
    Withdrawn {
        request_id: RequestId,
        at: TimestampMs,
    },
}

/// Folds the event log into requests with their final status, in id order.
pub fn fold_requests(events: &[GivenAmendmentEvent]) -> Vec<GivenAmendmentRequest> {
    let mut requests: Vec<GivenAmendmentRequest> = Vec::new();
    for event in events {
        match event {
            GivenAmendmentEvent::Proposed { request } => requests.push(request.clone()),
            GivenAmendmentEvent::Decided {
                request_id,
                accepted,
                reason,
                at,
            } => {
                if let Some(request) = requests.iter_mut().find(|r| r.id == *request_id) {
                    request.decision = Some(Decision {
                        accepted: *accepted,
                        reason: reason.clone(),
                        at: *at,
                    });
                }
            }
            GivenAmendmentEvent::Withdrawn { request_id, at } => {
                if let Some(request) = requests.iter_mut().find(|r| r.id == *request_id) {
                    request.withdrawn_at = Some(*at);
                }
            }
        }
    }
    requests.sort_by_key(|r| r.id);
    requests
}

/// The still-open request, if any (I-N1: at most one is acted on).
pub fn open_request(events: &[GivenAmendmentEvent]) -> Option<GivenAmendmentRequest> {
    fold_requests(events)
        .into_iter()
        .rev()
        .find(|r| r.is_open())
}

/// The individual's ruling on a proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GivenDecision {
    Accept { reason: String },
    Reject { reason: String },
}

/// Who rules on proposals. M1 ships a validation-only policy (I-N5); the
/// protocol and persistence are independent of which implementation is
/// wired, so a model-involved policy can replace it later.
pub trait NegotiationPolicy: Send + Sync {
    fn evaluate(
        &self,
        request: &GivenAmendmentRequest,
        current: &GivenFacts,
        today: DateYmd,
    ) -> GivenDecision;
}

/// Minimal usable M1 policy: a proposal is accepted only when it passes the
/// same field validation as initialization and differs from the current
/// value.
pub struct ValidationPolicy;

impl NegotiationPolicy for ValidationPolicy {
    fn evaluate(
        &self,
        request: &GivenAmendmentRequest,
        current: &GivenFacts,
        today: DateYmd,
    ) -> GivenDecision {
        let mut candidate = current.clone();
        if candidate
            .apply_field(request.field, request.proposed.clone())
            .is_err()
        {
            return GivenDecision::Reject {
                reason: "the field and the proposed value do not match".into(),
            };
        }
        if let Err(reason) = candidate.validate(today) {
            return GivenDecision::Reject { reason };
        }
        let unchanged = match request.field {
            GivenField::Name => candidate.name == current.name,
            GivenField::Birthday => candidate.birthday == current.birthday,
            GivenField::Gender => candidate.gender == current.gender,
            GivenField::InitialStyle => candidate.initial_style == current.initial_style,
        };
        if unchanged {
            return GivenDecision::Reject {
                reason: "the proposed value is already the current value".into(),
            };
        }
        GivenDecision::Accept {
            reason: "the proposal passes the same field validation as initialization".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> GivenFacts {
        GivenFacts {
            name: "Aria".into(),
            birthday: DateYmd::new(2026, 1, 15).expect("valid"),
            gender: Gender::Undisclosed,
            initial_style: "warm and terse".into(),
        }
    }

    fn request(field: GivenField, value: GivenValue) -> GivenAmendmentRequest {
        GivenAmendmentRequest::proposed(RequestId(1), field, value, "because".into(), 10)
    }

    #[test]
    fn status_derivation() {
        let mut r = request(GivenField::Name, GivenValue::Name("Vega".into()));
        assert_eq!(r.status(), RequestStatus::Proposed);
        assert!(r.is_open());
        r.withdrawn_at = Some(20);
        assert_eq!(r.status(), RequestStatus::Withdrawn);
        r.withdrawn_at = None;
        r.decision = Some(Decision {
            accepted: false,
            reason: "no".into(),
            at: 30,
        });
        assert_eq!(r.status(), RequestStatus::Rejected);
        r.decision = Some(Decision {
            accepted: true,
            reason: "yes".into(),
            at: 30,
        });
        assert_eq!(r.status(), RequestStatus::Accepted);
    }

    #[test]
    fn field_vocabulary_round_trips() {
        for field in [
            GivenField::Name,
            GivenField::Birthday,
            GivenField::Gender,
            GivenField::InitialStyle,
        ] {
            assert_eq!(GivenField::parse(field.as_str()), Some(field));
        }
        assert_eq!(GivenField::parse("nonsense"), None);
    }

    #[test]
    fn validation_policy_accepts_a_valid_change() {
        let decision = ValidationPolicy.evaluate(
            &request(GivenField::Name, GivenValue::Name("Vega".into())),
            &facts(),
            DateYmd::new(2026, 11, 3).expect("valid"),
        );
        assert!(matches!(decision, GivenDecision::Accept { .. }));
    }

    #[test]
    fn validation_policy_rejects_future_birthday_and_identical_value() {
        let today = DateYmd::new(2026, 11, 3).expect("valid");
        let future = ValidationPolicy.evaluate(
            &request(
                GivenField::Birthday,
                GivenValue::Birthday(DateYmd::new(2027, 1, 1).expect("valid")),
            ),
            &facts(),
            today,
        );
        assert!(matches!(future, GivenDecision::Reject { .. }));
        let identical = ValidationPolicy.evaluate(
            &request(GivenField::Name, GivenValue::Name("Aria".into())),
            &facts(),
            today,
        );
        assert!(matches!(identical, GivenDecision::Reject { .. }));
    }

    #[test]
    fn validation_policy_rejects_field_value_mismatch() {
        let decision = ValidationPolicy.evaluate(
            &request(GivenField::Name, GivenValue::Gender(Gender::Female)),
            &facts(),
            DateYmd::new(2026, 11, 3).expect("valid"),
        );
        assert!(matches!(decision, GivenDecision::Reject { .. }));
    }

    #[test]
    fn fold_reconstructs_requests_and_open_one() {
        let events = vec![
            GivenAmendmentEvent::Proposed {
                request: request(GivenField::Name, GivenValue::Name("Vega".into())),
            },
            GivenAmendmentEvent::Decided {
                request_id: RequestId(1),
                accepted: true,
                reason: "ok".into(),
                at: 20,
            },
            GivenAmendmentEvent::Proposed {
                request: GivenAmendmentRequest::proposed(
                    RequestId(2),
                    GivenField::Gender,
                    GivenValue::Gender(Gender::Female),
                    "why not".into(),
                    30,
                ),
            },
        ];
        let folded = fold_requests(&events);
        assert_eq!(folded.len(), 2);
        assert_eq!(folded[0].status(), RequestStatus::Accepted);
        assert!(open_request(&events).is_some_and(|r| r.id.0 == 2));
    }
}
