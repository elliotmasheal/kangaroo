//! The UserProfile document model: TOML front matter for the five fact
//! sections; the body is the user's own notes, preserved verbatim on every
//! rewrite. This is the one document a user may hand-edit (I-U2).

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::domain::profile::{SectionFacts, UserProfile};
use crate::infra::doc_store::DocumentModel;

#[derive(Debug, Serialize, Deserialize)]
struct ProfileFrontMatter {
    schema: u32,
    identity: SectionFacts,
    environment: SectionFacts,
    preferences: SectionFacts,
    goals: SectionFacts,
    corrections: SectionFacts,
}

impl DocumentModel for UserProfile {
    const SCHEMA_VERSION: u32 = 1;
    const DOC_NAME: &'static str = "user_profile.md";
    const LOG_NAME: &'static str = "user_profile.revisions.jsonl";

    fn front_matter(&self) -> Result<String> {
        let front = ProfileFrontMatter {
            schema: Self::SCHEMA_VERSION,
            identity: self.identity.clone(),
            environment: self.environment.clone(),
            preferences: self.preferences.clone(),
            goals: self.goals.clone(),
            corrections: self.corrections.clone(),
        };
        Ok(toml::to_string(&front)?)
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
        Ok(Self {
            identity: front.identity,
            environment: front.environment,
            preferences: front.preferences,
            goals: front.goals,
            corrections: front.corrections,
            notes: body.to_owned(),
        })
    }

    fn body(&self) -> String {
        self.notes.clone()
    }
}

// A fact's front-matter shape ([identity] tables with inline
// `value/source/updated_at` entries) is owned by ProfileFact's serde derive.
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
}
