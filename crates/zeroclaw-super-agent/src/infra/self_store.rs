//! The Self document model: TOML front matter for given/earned/observations;
//! the body is a regenerated narrative (the document is product-owned, so
//! there is no user free-text area to preserve).

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::domain::selfhood::{EarnedProfile, GivenFacts, ObservationCounter, SelfState};
use crate::infra::doc_store::DocumentModel;

#[derive(Debug, Serialize, Deserialize)]
struct SelfFrontMatter {
    schema: u32,
    given: GivenFacts,
    earned: EarnedProfile,
    observations: BTreeMap<String, ObservationCounter>,
}

impl DocumentModel for SelfState {
    const SCHEMA_VERSION: u32 = 1;
    const DOC_NAME: &'static str = "self.md";
    const LOG_NAME: &'static str = "self.revisions.jsonl";

    fn front_matter(&self) -> Result<String> {
        let front = SelfFrontMatter {
            schema: Self::SCHEMA_VERSION,
            given: self.given.clone(),
            earned: self.earned.clone(),
            observations: self.observations.clone(),
        };
        Ok(toml::to_string(&front)?)
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
        Ok(Self {
            given: front.given,
            earned: front.earned,
            observations: front.observations,
        })
    }

    fn body(&self) -> String {
        self.render_narrative()
    }
}
