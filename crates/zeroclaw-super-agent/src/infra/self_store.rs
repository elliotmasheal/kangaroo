//! The Self document model: TOML front matter for given/earned/observations;
//! the body is a regenerated narrative (the document is product-owned, so
//! there is no user free-text area to preserve). Serialization lives here in
//! infra-owned DTOs; the domain aggregate stays serialization-free.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::domain::selfhood::{
    EarnedProfile, EarnedTrait, Gender, GivenFacts, ObservationCounter, SelfState,
};
use crate::infra::doc_store::DocumentModel;

#[derive(Debug, Serialize, Deserialize)]
struct SelfFrontMatter {
    schema: u32,
    #[serde(flatten)]
    given: GivenDto,
    earned: EarnedDto,
    observations: BTreeMap<String, ObservationDto>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GivenDto {
    name: String,
    birthday: String,
    gender: String,
    initial_style: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct EarnedDto {
    #[serde(rename = "traits")]
    traits: Vec<TraitDto>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TraitDto {
    kind: String,
    strength: u8,
    evidence_count: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct ObservationDto {
    count: u32,
    last_seen: u64,
}

impl From<&GivenFacts> for GivenDto {
    fn from(given: &GivenFacts) -> Self {
        Self {
            name: given.name.clone(),
            birthday: given.birthday.to_string(),
            gender: given.gender.as_str().to_owned(),
            initial_style: given.initial_style.clone(),
        }
    }
}

impl TryFrom<GivenDto> for GivenFacts {
    type Error = String;

    fn try_from(dto: GivenDto) -> Result<Self, Self::Error> {
        Ok(Self {
            name: dto.name,
            birthday: crate::domain::DateYmd::parse(&dto.birthday)?,
            gender: Gender::parse(&dto.gender)
                .ok_or_else(|| format!("unknown gender: {}", dto.gender))?,
            initial_style: dto.initial_style,
        })
    }
}

impl From<&EarnedProfile> for EarnedDto {
    fn from(earned: &EarnedProfile) -> Self {
        Self {
            traits: earned
                .traits
                .iter()
                .map(|t| TraitDto {
                    kind: t.kind.clone(),
                    strength: t.strength,
                    evidence_count: t.evidence_count,
                })
                .collect(),
        }
    }
}

impl TryFrom<EarnedDto> for EarnedProfile {
    type Error = String;

    fn try_from(dto: EarnedDto) -> Result<Self, Self::Error> {
        Ok(Self {
            traits: dto
                .traits
                .into_iter()
                .map(|t| {
                    Ok(EarnedTrait {
                        kind: t.kind,
                        strength: t.strength,
                        evidence_count: t.evidence_count,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?,
        })
    }
}

impl From<&SelfState> for SelfFrontMatter {
    fn from(state: &SelfState) -> Self {
        Self {
            schema: SelfState::SCHEMA_VERSION,
            given: GivenDto::from(&state.given),
            earned: EarnedDto::from(&state.earned),
            observations: state
                .observations
                .iter()
                .map(|(kind, counter)| {
                    (
                        kind.clone(),
                        ObservationDto {
                            count: counter.count,
                            last_seen: counter.last_seen,
                        },
                    )
                })
                .collect(),
        }
    }
}

impl TryFrom<SelfFrontMatter> for SelfState {
    type Error = String;

    fn try_from(front: SelfFrontMatter) -> Result<Self, Self::Error> {
        Ok(Self {
            given: GivenFacts::try_from(front.given)?,
            earned: EarnedProfile::try_from(front.earned)?,
            observations: front
                .observations
                .into_iter()
                .map(|(kind, dto)| {
                    (
                        kind,
                        ObservationCounter {
                            count: dto.count,
                            last_seen: dto.last_seen,
                        },
                    )
                })
                .collect(),
        })
    }
}

impl DocumentModel for SelfState {
    const SCHEMA_VERSION: u32 = 1;
    const DOC_NAME: &'static str = "self.md";
    const LOG_NAME: &'static str = "self.revisions.jsonl";

    fn front_matter(&self) -> Result<String> {
        Ok(toml::to_string(&SelfFrontMatter::from(self))?)
    }

    fn from_parts(front_matter: &str, _body: &str) -> Result<Self> {
        let front: SelfFrontMatter = toml::from_str(front_matter)?;
        if front.schema != Self::SCHEMA_VERSION {
            bail!(
                "{} has schema {} but this build understands {}",
                Self::DOC_NAME,
                front.schema,
                Self::SCHEMA_VERSION
            );
        }
        SelfState::try_from(front).map_err(anyhow::Error::msg)
    }

    fn body(&self) -> String {
        self.render_narrative()
    }
}
