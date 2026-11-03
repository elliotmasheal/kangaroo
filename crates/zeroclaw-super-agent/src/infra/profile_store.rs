//! The UserProfile document model: TOML front matter for the five fact
//! sections; the body is the user's own notes, preserved verbatim on every
//! rewrite. This is the one document a user may hand-edit (I-U2).
//! Serialization lives here in infra-owned DTOs; the domain aggregate stays
//! serialization-free.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::domain::profile::{ChangeSource, ProfileFact, SectionFacts, SectionKind, UserProfile};
use crate::infra::doc_store::DocumentModel;

#[derive(Debug, Serialize, Deserialize)]
struct ProfileFrontMatter {
    schema: u32,
    identity: BTreeMap<String, FactDto>,
    environment: BTreeMap<String, FactDto>,
    preferences: BTreeMap<String, FactDto>,
    goals: BTreeMap<String, FactDto>,
    corrections: BTreeMap<String, FactDto>,
}

#[derive(Debug, Serialize, Deserialize)]
struct FactDto {
    value: String,
    source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    confidence: Option<u8>,
    updated_at: u64,
}

fn section_to_dto(section: &SectionFacts) -> BTreeMap<String, FactDto> {
    section
        .iter()
        .map(|(key, fact)| {
            (
                key.clone(),
                FactDto {
                    value: fact.value.clone(),
                    source: fact.source.kind().to_owned(),
                    confidence: fact.source.confidence(),
                    updated_at: fact.updated_at,
                },
            )
        })
        .collect()
}

fn section_from_dto(raw: BTreeMap<String, FactDto>, section: SectionKind) -> Result<SectionFacts> {
    raw.into_iter()
        .map(|(key, dto)| {
            let source =
                ChangeSource::from_parts(&dto.source, dto.confidence).map_err(|reason| {
                    anyhow::Error::msg(format!("{}/{key}: {reason}", section.as_str()))
                })?;
            Ok((
                key,
                ProfileFact {
                    value: dto.value,
                    source,
                    updated_at: dto.updated_at,
                },
            ))
        })
        .collect()
}

impl From<&UserProfile> for ProfileFrontMatter {
    fn from(profile: &UserProfile) -> Self {
        Self {
            schema: UserProfile::SCHEMA_VERSION,
            identity: section_to_dto(&profile.identity),
            environment: section_to_dto(&profile.environment),
            preferences: section_to_dto(&profile.preferences),
            goals: section_to_dto(&profile.goals),
            corrections: section_to_dto(&profile.corrections),
        }
    }
}

impl TryFrom<ProfileFrontMatter> for UserProfile {
    type Error = anyhow::Error;

    fn try_from(front: ProfileFrontMatter) -> Result<Self> {
        Ok(Self {
            identity: section_from_dto(front.identity, SectionKind::Identity)?,
            environment: section_from_dto(front.environment, SectionKind::Environment)?,
            preferences: section_from_dto(front.preferences, SectionKind::Preferences)?,
            goals: section_from_dto(front.goals, SectionKind::Goals)?,
            corrections: section_from_dto(front.corrections, SectionKind::Corrections)?,
            notes: String::new(),
        })
    }
}

impl DocumentModel for UserProfile {
    const SCHEMA_VERSION: u32 = 1;
    const DOC_NAME: &'static str = "user_profile.md";
    const LOG_NAME: &'static str = "user_profile.revisions.jsonl";

    fn front_matter(&self) -> Result<String> {
        Ok(toml::to_string(&ProfileFrontMatter::from(self))?)
    }

    fn from_parts(front_matter: &str, body: &str) -> Result<Self> {
        let front: ProfileFrontMatter = toml::from_str(front_matter)?;
        if front.schema != Self::SCHEMA_VERSION {
            bail!(
                "{} has schema {} but this build understands {}",
                Self::DOC_NAME,
                front.schema,
                Self::SCHEMA_VERSION
            );
        }
        let mut profile = UserProfile::try_from(front)?;
        profile.notes = body.to_owned();
        Ok(profile)
    }

    fn body(&self) -> String {
        self.notes.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::profile::{ChangeSource, ProfileFact};

    #[test]
    fn front_matter_round_trip_preserves_facts_and_notes() {
        let mut profile = UserProfile {
            notes: "# my notes\n\nkept verbatim".into(),
            ..Default::default()
        };
        profile.preferences.insert(
            "editor".into(),
            ProfileFact {
                value: "Neovim".into(),
                source: ChangeSource::AgentInference { confidence: 80 },
                updated_at: 42,
            },
        );
        let front = DocumentModel::front_matter(&profile).expect("renders");
        let body = DocumentModel::body(&profile);
        let restored = <UserProfile as DocumentModel>::from_parts(&front, &body).expect("parses");
        assert_eq!(restored, profile);
    }

    #[test]
    fn unknown_schema_is_rejected() {
        let raw = format!("schema = {}\n", UserProfile::SCHEMA_VERSION + 1);
        assert!(<UserProfile as DocumentModel>::from_parts(&raw, "").is_err());
    }

    #[test]
    fn inference_without_confidence_is_rejected() {
        let raw = "[preferences]\neditor = { value = \"x\", source = \"agent-inference\", updated_at = 1 }\n";
        assert!(<UserProfile as DocumentModel>::from_parts(raw, "").is_err());
    }

    #[test]
    fn unknown_source_kind_is_rejected() {
        let raw =
            "[preferences]\neditor = { value = \"x\", source = \"nonsense\", updated_at = 1 }\n";
        assert!(<UserProfile as DocumentModel>::from_parts(raw, "").is_err());
    }
}
