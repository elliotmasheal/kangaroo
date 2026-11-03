//! The agent-callable tools over the UserProfile aggregate. The tool layer
//! only translates: each external call becomes one domain operation through
//! the application service; no business rule lives here.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use serde_json::json;
use zeroclaw_api::tool::{Tool, ToolResult};

use crate::application::profile_service::ProfileService;
use crate::domain::RevisionId;
use crate::domain::profile::{
    IgnoreReason, Observation, ObservationKind, RecordDecision, SectionKind,
};
use zeroclaw_runtime::composition::{
    NativeRecipeInputs, NativeRegistryRecipe, ToolRequest, ToolSource,
};
use zeroclaw_runtime::tools::AllToolsResult;

/// English fallbacks matching the repo pattern (see `read_skill.rs`): the
/// localized description comes from the Fluent catalogue via the runtime
/// i18n lookup, the constant covers builds where the key is absent.
const UPDATE_DESCRIPTION_EN: &str = "Record observations about the user into the user profile. Each observation carries a section, a short key, the fact value, an observation kind, and a confidence. The profile decides what is worth recording; credentials, transient notes, and duplicates are refused.";
const ROLLBACK_DESCRIPTION_EN: &str = "Roll the user profile back to an earlier revision by revision number. The rollback itself is recorded as a new revision.";

fn localized_description(name: &str, fallback: &'static str) -> String {
    zeroclaw_runtime::i18n::get_tool_description(name)
        .map(str::to_owned)
        .unwrap_or_else(|| fallback.to_owned())
}

fn observation_from_args(value: &serde_json::Value) -> anyhow::Result<Observation> {
    let section = value
        .get("section")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::Error::msg("each observation needs a section"))?;
    let section = SectionKind::parse(section)
        .ok_or_else(|| anyhow::Error::msg(format!("unknown profile section: {section}")))?;
    let key = value
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::Error::msg("each observation needs a key"))?
        .to_owned();
    let fact_value = value
        .get("value")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::Error::msg("each observation needs a value"))?
        .to_owned();
    let kind = match value.get("kind").and_then(|v| v.as_str()) {
        Some(raw) => ObservationKind::parse(raw)
            .ok_or_else(|| anyhow::Error::msg(format!("unknown observation kind: {raw}")))?,
        None => ObservationKind::StableFact,
    };
    let confidence = value
        .get("confidence")
        .and_then(|v| v.as_u64())
        .unwrap_or(75);
    let confidence = u8::try_from(confidence)
        .map_err(|_| anyhow::Error::msg("confidence must fit in 0..=100"))?;
    Ok(Observation {
        section,
        key,
        value: fact_value,
        kind,
        confidence,
    })
}

fn decision_label(decision: &RecordDecision) -> serde_json::Value {
    match decision {
        RecordDecision::Record => json!("recorded"),
        RecordDecision::NeedsReview => json!("needs_review"),
        RecordDecision::Ignore { reason } => json!(format!(
            "ignored: {}",
            match reason {
                IgnoreReason::EmptyValue => "empty value",
                IgnoreReason::Transient => "transient context",
                IgnoreReason::CredentialLike => "credential-like content",
                IgnoreReason::LowConfidence => "low confidence",
                IgnoreReason::Duplicate => "duplicate of the current fact",
                IgnoreReason::Superseded => {
                    "kept the existing higher-priority or newer fact"
                }
                IgnoreReason::CapacityFull => "the section is full; retire a fact first",
            }
        )),
    }
}

/// Records user observations into the UserProfile. Translation only: the
/// arguments become domain observations, the recording plan becomes the
/// structured output. Self is unreachable from this type by construction.
pub struct ProfileUpdateTool {
    service: Arc<ProfileService>,
    description: OnceLock<String>,
}

impl ProfileUpdateTool {
    pub fn new(service: Arc<ProfileService>) -> Self {
        Self {
            service,
            description: OnceLock::new(),
        }
    }
}

zeroclaw_api::tool_attribution!(
    ProfileUpdateTool,
    zeroclaw_api::attribution::ToolKind::Memory
);

#[async_trait]
impl Tool for ProfileUpdateTool {
    fn name(&self) -> &str {
        "profile_update"
    }

    fn description(&self) -> &str {
        self.description
            .get_or_init(|| localized_description(self.name(), UPDATE_DESCRIPTION_EN))
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "observations": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "section": { "type": "string", "enum": ["identity", "environment", "preferences", "goals", "corrections"] },
                            "key": { "type": "string" },
                            "value": { "type": "string" },
                            "kind": { "type": "string", "enum": ["correction", "stable_fact", "goal_signal", "transient"] },
                            "confidence": { "type": "integer", "minimum": 0, "maximum": 100 }
                        },
                        "required": ["section", "key", "value"]
                    }
                }
            },
            "required": ["observations"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let raw = args
            .get("observations")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::Error::msg("arguments must carry an observations array"))?;
        let mut observations = Vec::with_capacity(raw.len());
        for value in raw {
            observations.push(observation_from_args(value)?);
        }
        if observations.is_empty() {
            return Ok(ToolResult::err("the observations array is empty"));
        }
        let plan = self.service.observe_batch(&observations)?;
        let decisions: Vec<serde_json::Value> = plan
            .outcomes
            .iter()
            .map(|outcome| {
                json!({
                    "section": outcome.section.as_str(),
                    "key": outcome.key,
                    "decision": decision_label(&outcome.decision),
                })
            })
            .collect();
        let fingerprint = self.service.current()?.fingerprint.0;
        Ok(ToolResult::ok(json!({
            "decisions": decisions,
            "committed": !plan.patches.is_empty(),
            "profile_fingerprint": fingerprint,
        })))
    }
}

/// Rolls the UserProfile back to an earlier revision. Translation only: the
/// revision argument becomes one domain rollback through the service, and
/// the rollback leaves its own revision behind (I-U7).
pub struct ProfileRollbackTool {
    service: Arc<ProfileService>,
    description: OnceLock<String>,
}

impl ProfileRollbackTool {
    pub fn new(service: Arc<ProfileService>) -> Self {
        Self {
            service,
            description: OnceLock::new(),
        }
    }
}

zeroclaw_api::tool_attribution!(
    ProfileRollbackTool,
    zeroclaw_api::attribution::ToolKind::Memory
);

#[async_trait]
impl Tool for ProfileRollbackTool {
    fn name(&self) -> &str {
        "profile_rollback"
    }

    fn description(&self) -> &str {
        self.description
            .get_or_init(|| localized_description(self.name(), ROLLBACK_DESCRIPTION_EN))
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "revision": { "type": "integer", "minimum": 1 }
            },
            "required": ["revision"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let raw_revision = args
            .get("revision")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow::Error::msg("arguments must carry a revision number"))?;
        if raw_revision == 0 {
            return Ok(ToolResult::err("revision numbers start at 1"));
        }
        let receipt = self.service.rollback(RevisionId(raw_revision))?;
        Ok(ToolResult::ok(json!({
            "rolled_back_to": raw_revision,
            "revision": receipt.revision.0,
        })))
    }
}

/// Supplies the profile tools to the runtime's registry seam through the
/// application-owned [`ToolSource`] port. Two modes:
///
/// - standalone: no native recipe — the registry is exactly the profile
///   tools (the M2 product surface);
/// - wrapper: a native recipe (typically `composition::defaults::native_compat_registry`)
///   builds the native registry and the profile tools are appended to it;
///   a native tool whose name a profile tool shares wins, and the agent's
///   policy plus the caller allowlist still filter everything.
///
/// Either way the tools land behind the runtime's existing admission gates;
/// nothing here bypasses policy, and no runtime file changes.
pub struct ProfileToolsSource {
    profile_service: Arc<ProfileService>,
    native: Option<Box<NativeRegistryRecipe>>,
}

impl ProfileToolsSource {
    /// Standalone source: the registry is the profile tools only.
    pub fn standalone(profile_service: Arc<ProfileService>) -> Self {
        Self {
            profile_service,
            native: None,
        }
    }

    /// Wrapping source: the native registry plus the profile tools.
    pub fn wrapping(
        profile_service: Arc<ProfileService>,
        native: impl Fn(NativeRecipeInputs<'_>) -> anyhow::Result<AllToolsResult>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            profile_service,
            native: Some(Box::new(native)),
        }
    }
}

impl ToolSource for ProfileToolsSource {
    fn native_recipe<'s>(
        &'s self,
        _request: &ToolRequest<'_>,
    ) -> anyhow::Result<Option<&'s NativeRegistryRecipe>> {
        Ok(self.native.as_deref())
    }

    fn tools(&self, _request: &ToolRequest<'_>) -> anyhow::Result<Vec<Box<dyn Tool>>> {
        Ok(vec![
            Box::new(ProfileUpdateTool::new(Arc::clone(&self.profile_service))),
            Box::new(ProfileRollbackTool::new(Arc::clone(&self.profile_service))),
        ])
    }
}
