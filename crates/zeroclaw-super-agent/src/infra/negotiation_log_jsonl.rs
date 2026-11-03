//! Append-only JSONL event log for Given negotiation (I-N4). Independent
//! from both document revision logs; a rejected or withdrawn negotiation
//! produces no document revision (I-N3). Events map through infra-owned DTOs;
//! the domain event type stays serialization-free.

use anyhow::{Context, Result};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use crate::application::ports::NegotiationEventLog;
use crate::domain::DateYmd;
use crate::domain::selfhood::Gender;
use crate::domain::selfhood::negotiation::{
    Decision, GivenAmendmentEvent, GivenAmendmentRequest, GivenField, GivenValue, RequestId,
};

pub(crate) const NEGOTIATION_LOG_NAME: &str = "given_negotiation.jsonl";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum EventDto {
    Proposed {
        request: RequestDto,
    },
    Decided {
        request_id: u64,
        accepted: bool,
        reason: String,
        at: u64,
    },
    Withdrawn {
        request_id: u64,
        at: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RequestDto {
    id: u64,
    field: String,
    proposed: ValueDto,
    reason_from_user: String,
    opened_at: u64,
    decision: Option<DecisionDto>,
    withdrawn_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DecisionDto {
    accepted: bool,
    reason: String,
    at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ValueDto {
    field: String,
    name: Option<String>,
    birthday: Option<String>,
    gender: Option<String>,
    initial_style: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LogLine {
    event: EventDto,
}

impl RequestDto {
    fn from_domain(request: &GivenAmendmentRequest) -> Self {
        Self {
            id: request.id.0,
            field: request.field.as_str().to_owned(),
            proposed: ValueDto::from_domain(request.field, &request.proposed),
            reason_from_user: request.reason_from_user.clone(),
            opened_at: request.opened_at,
            decision: request.decision.as_ref().map(|d| DecisionDto {
                accepted: d.accepted,
                reason: d.reason.clone(),
                at: d.at,
            }),
            withdrawn_at: request.withdrawn_at,
        }
    }

    fn into_domain(self) -> Result<GivenAmendmentRequest> {
        let field = GivenField::parse(&self.field)
            .ok_or_else(|| anyhow::Error::msg(format!("unknown given field: {}", self.field)))?;
        Ok(GivenAmendmentRequest {
            id: RequestId(self.id),
            field,
            proposed: self.proposed.into_domain(field)?,
            reason_from_user: self.reason_from_user,
            opened_at: self.opened_at,
            decision: self.decision.map(|d| Decision {
                accepted: d.accepted,
                reason: d.reason,
                at: d.at,
            }),
            withdrawn_at: self.withdrawn_at,
        })
    }
}

impl ValueDto {
    fn from_domain(field: GivenField, value: &GivenValue) -> Self {
        let mut dto = Self {
            field: field.as_str().to_owned(),
            name: None,
            birthday: None,
            gender: None,
            initial_style: None,
        };
        match (field, value) {
            (GivenField::Name, GivenValue::Name(v)) => dto.name = Some(v.clone()),
            (GivenField::Birthday, GivenValue::Birthday(v)) => dto.birthday = Some(v.to_string()),
            (GivenField::Gender, GivenValue::Gender(v)) => dto.gender = Some(v.as_str().to_owned()),
            (GivenField::InitialStyle, GivenValue::InitialStyle(v)) => {
                dto.initial_style = Some(v.clone())
            }
            _ => {}
        }
        dto
    }

    fn into_domain(self, field: GivenField) -> Result<GivenValue> {
        let mismatch = || anyhow::Error::msg("the stored value does not match its field");
        Ok(match field {
            GivenField::Name => GivenValue::Name(self.name.ok_or_else(mismatch)?),
            GivenField::Birthday => GivenValue::Birthday(
                DateYmd::parse(&self.birthday.ok_or_else(mismatch)?).map_err(anyhow::Error::msg)?,
            ),
            GivenField::Gender => GivenValue::Gender(
                Gender::parse(&self.gender.ok_or_else(mismatch)?)
                    .ok_or_else(|| anyhow::Error::msg("unknown gender in negotiation log"))?,
            ),
            GivenField::InitialStyle => {
                GivenValue::InitialStyle(self.initial_style.ok_or_else(mismatch)?)
            }
        })
    }
}

fn dto_from_domain(event: &GivenAmendmentEvent) -> LogLine {
    let dto = match event {
        GivenAmendmentEvent::Proposed { request } => EventDto::Proposed {
            request: RequestDto::from_domain(request),
        },
        GivenAmendmentEvent::Decided {
            request_id,
            accepted,
            reason,
            at,
        } => EventDto::Decided {
            request_id: request_id.0,
            accepted: *accepted,
            reason: reason.clone(),
            at: *at,
        },
        GivenAmendmentEvent::Withdrawn { request_id, at } => EventDto::Withdrawn {
            request_id: request_id.0,
            at: *at,
        },
    };
    LogLine { event: dto }
}

fn domain_from_dto(line: &LogLine) -> Result<GivenAmendmentEvent> {
    Ok(match &line.event {
        EventDto::Proposed { request } => GivenAmendmentEvent::Proposed {
            request: request.clone().into_domain()?,
        },
        EventDto::Decided {
            request_id,
            accepted,
            reason,
            at,
        } => GivenAmendmentEvent::Decided {
            request_id: RequestId(*request_id),
            accepted: *accepted,
            reason: reason.clone(),
            at: *at,
        },
        EventDto::Withdrawn { request_id, at } => GivenAmendmentEvent::Withdrawn {
            request_id: RequestId(*request_id),
            at: *at,
        },
    })
}

pub(crate) struct JsonlNegotiationLog {
    dir: Mutex<Dir>,
}

impl JsonlNegotiationLog {
    pub(crate) fn new(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)
            .with_context(|| format!("failed to create the document root {}", root.display()))?;
        let dir = Dir::open_ambient_dir(root, ambient_authority())?;
        Ok(Self {
            dir: Mutex::new(dir),
        })
    }

    fn lock_dir(&self) -> MutexGuard<'_, Dir> {
        self.dir
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl NegotiationEventLog for JsonlNegotiationLog {
    fn append(&self, event: &GivenAmendmentEvent) -> Result<()> {
        let line = serde_json::to_string(&dto_from_domain(event))?;
        let dir = self.lock_dir();
        let mut file = dir.open_with(
            NEGOTIATION_LOG_NAME,
            OpenOptions::new().write(true).create(true).append(true),
        )?;
        writeln!(file, "{line}")?;
        file.sync_all()?;
        Ok(())
    }

    fn load_all(&self) -> Result<Vec<GivenAmendmentEvent>> {
        let dir = self.lock_dir();
        if !dir.exists(NEGOTIATION_LOG_NAME) {
            return Ok(Vec::new());
        }
        let mut contents = String::new();
        dir.open(NEGOTIATION_LOG_NAME)?
            .read_to_string(&mut contents)
            .with_context(|| format!("failed to read {NEGOTIATION_LOG_NAME}"))?;
        let mut events = Vec::new();
        for (index, line) in contents.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            events.push(
                domain_from_dto(&serde_json::from_str::<LogLine>(line).with_context(|| {
                    format!("corrupt negotiation log entry at line {}", index + 1)
                })?)
                .with_context(|| format!("negotiation log entry at line {}", index + 1))?,
            );
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proposed_line_round_trips_through_the_dto() {
        let event = GivenAmendmentEvent::Proposed {
            request: GivenAmendmentRequest::proposed(
                RequestId(1),
                GivenField::Name,
                GivenValue::Name("Vega".into()),
                "crash mid-negotiation".into(),
                1,
            ),
        };
        let line = serde_json::to_string(&dto_from_domain(&event)).expect("serializes");
        let parsed: LogLine = serde_json::from_str(&line).expect("the DTO line parses");
        let restored = domain_from_dto(&parsed).expect("maps back");
        assert_eq!(restored, event);
    }

    #[test]
    fn a_handwritten_proposed_line_parses() {
        let raw = concat!(
            r#"{"event":{"type":"proposed","request":{"id":1,"field":"name","#,
            r#""proposed":{"field":"name","name":"Vega","birthday":null,"gender":null,"initial_style":null},"#,
            r#""reason_from_user":"crash mid-negotiation","opened_at":1,"decision":null,"withdrawn_at":null}}}"#,
        );
        let parsed: LogLine = serde_json::from_str(raw).expect("the handwritten line parses");
        let restored = domain_from_dto(&parsed).expect("maps back");
        assert!(matches!(restored, GivenAmendmentEvent::Proposed { .. }));
    }
}
