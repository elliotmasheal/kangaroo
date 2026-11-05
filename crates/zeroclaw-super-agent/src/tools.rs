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
use crate::infra::pending::{PendingObservation, PendingQueue};
use zeroclaw_runtime::composition::{
    NativeRecipeInputs, NativeRegistryRecipe, ToolRequest, ToolSource,
};
use zeroclaw_runtime::tools::AllToolsResult;

/// English fallbacks matching the repo pattern (see `read_skill.rs`): the
/// localized description comes from the Fluent catalogue via the runtime
/// i18n lookup, the constant covers builds where the key is absent.
const UPDATE_DESCRIPTION_EN: &str = "Record observations about the user into the user profile. Each observation carries a section, a short key, the fact value, an observation kind, and a confidence. The profile decides what is worth recording; credentials, transient notes, and duplicates are refused. Inferred observations (any kind other than correction) do not enter the profile directly: they are queued for the nightly consolidation pass, which adjudicates them against episode evidence before anything is recorded.";
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
///
/// I-DR1: when a pending queue is configured, only user corrections write
/// straight through; every other (inferred) observation is queued for the
/// nightly dream's promotion stage, which adjudicates it against episode
/// evidence before anything enters the profile.
pub struct ProfileUpdateTool {
    service: Arc<ProfileService>,
    pending_queue: Option<Arc<PendingQueue>>,
    description: OnceLock<String>,
}

impl ProfileUpdateTool {
    pub fn new(service: Arc<ProfileService>) -> Self {
        Self {
            service,
            pending_queue: None,
            description: OnceLock::new(),
        }
    }

    /// Attaches the inferred-observation queue (the I-DR1 path). Without it
    /// the tool keeps its standalone behaviour: everything writes directly.
    pub fn with_pending_queue(mut self, queue: Arc<PendingQueue>) -> Self {
        self.pending_queue = Some(queue);
        self
    }

    /// The pre-queue behaviour: the whole batch goes through the aggregate's
    /// recording plan and every decision is reported back.
    fn execute_direct(&self, observations: Vec<Observation>) -> anyhow::Result<ToolResult> {
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
        let Some(queue) = &self.pending_queue else {
            // Standalone path (no queue configured): everything writes
            // directly, exactly as before the I-DR1 switch.
            return self.execute_direct(observations);
        };
        // I-DR1: user corrections are user-confirmed material and still write
        // straight through; every other kind is agent inference and queues
        // for the dream's promotion stage instead of touching the profile.
        let enqueued_at = chrono::Local::now().to_rfc3339();
        let mut corrections: Vec<(usize, Observation)> = Vec::new();
        let mut queued: Vec<(usize, PendingObservation)> = Vec::new();
        for (index, observation) in observations.into_iter().enumerate() {
            if matches!(observation.kind, ObservationKind::Correction) {
                corrections.push((index, observation));
            } else {
                queued.push((
                    index,
                    PendingObservation {
                        section: observation.section.as_str().to_owned(),
                        key: observation.key.clone(),
                        value: observation.value.clone(),
                        confidence: observation.confidence,
                        enqueued_at: enqueued_at.clone(),
                    },
                ));
            }
        }
        let queued_items: Vec<PendingObservation> =
            queued.iter().map(|(_, item)| item.clone()).collect();
        queue.append(&queued_items)?;
        let plan = if corrections.is_empty() {
            None
        } else {
            let batch: Vec<Observation> = corrections
                .iter()
                .map(|(_, observation)| observation.clone())
                .collect();
            Some(self.service.observe_batch(&batch)?)
        };
        let mut decisions: Vec<Option<serde_json::Value>> =
            vec![None; queued.len() + corrections.len()];
        for (index, item) in &queued {
            decisions[*index] = Some(json!({
                "section": item.section,
                "key": item.key,
                "decision": "queued",
            }));
        }
        if let Some(plan) = &plan {
            for ((index, _), outcome) in corrections.iter().zip(plan.outcomes.iter()) {
                decisions[*index] = Some(json!({
                    "section": outcome.section.as_str(),
                    "key": outcome.key,
                    "decision": decision_label(&outcome.decision),
                }));
            }
        }
        let decisions: Vec<serde_json::Value> = decisions
            .into_iter()
            .map(|entry| entry.unwrap_or_else(|| json!({"decision": "unknown"})))
            .collect();
        let fingerprint = self.service.current()?.fingerprint.0;
        Ok(ToolResult::ok(json!({
            "decisions": decisions,
            "committed": plan.as_ref().is_some_and(|plan| !plan.patches.is_empty()),
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
    pending_queue: Option<Arc<PendingQueue>>,
    native: Option<Box<NativeRegistryRecipe>>,
}

impl ProfileToolsSource {
    /// Standalone source: the registry is the profile tools only.
    pub fn standalone(profile_service: Arc<ProfileService>) -> Self {
        Self {
            profile_service,
            pending_queue: None,
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
            pending_queue: None,
            native: Some(Box::new(native)),
        }
    }

    /// Routes inferred observations through the I-DR1 queue: corrections
    /// still write directly, everything else waits for the dream's promotion
    /// stage.
    pub fn with_pending_queue(mut self, queue: Arc<PendingQueue>) -> Self {
        self.pending_queue = Some(queue);
        self
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
        let mut update = ProfileUpdateTool::new(Arc::clone(&self.profile_service));
        if let Some(queue) = &self.pending_queue {
            update = update.with_pending_queue(Arc::clone(queue));
        }
        Ok(vec![
            Box::new(update),
            Box::new(ProfileRollbackTool::new(Arc::clone(&self.profile_service))),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::pending::pending_observations_path;
    use std::path::Path;

    fn service(root: &Path) -> Arc<ProfileService> {
        Arc::new(ProfileService::new(
            crate::infra::markdown_profile_store(root).expect("profile store"),
            crate::infra::system_clock(),
        ))
    }

    fn args(observations: serde_json::Value) -> serde_json::Value {
        json!({ "observations": observations })
    }

    fn decisions(result: &ToolResult) -> Vec<serde_json::Value> {
        assert!(result.success, "tool call succeeds: {:?}", result.error);
        let output = result.output.data().expect("structured output");
        output["decisions"]
            .as_array()
            .expect("decisions array")
            .clone()
    }

    #[tokio::test]
    async fn without_a_queue_everything_writes_directly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = service(dir.path());
        let tool = ProfileUpdateTool::new(Arc::clone(&service));
        let result = tool
            .execute(args(json!([{
                "section": "preferences", "key": "editor", "value": "Neovim",
                "kind": "stable_fact", "confidence": 80
            }])))
            .await
            .expect("execute");
        let decisions = decisions(&result);
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0]["decision"], "recorded");
        let profile = service.current().expect("load").state.expect("profile");
        assert_eq!(
            profile
                .fact(SectionKind::Preferences, "editor")
                .expect("fact written")
                .value,
            "Neovim"
        );
    }

    #[tokio::test]
    async fn with_a_queue_inference_queues_and_corrections_write_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = service(dir.path());
        let queue = Arc::new(PendingQueue::new(pending_observations_path(dir.path())));
        let tool =
            ProfileUpdateTool::new(Arc::clone(&service)).with_pending_queue(Arc::clone(&queue));
        let result = tool
            .execute(args(json!([
                {
                    "section": "preferences", "key": "editor", "value": "Neovim",
                    "kind": "stable_fact", "confidence": 80
                },
                {
                    "section": "preferences", "key": "tone", "value": "直接点",
                    "kind": "correction"
                },
                {
                    "section": "goals", "key": "trip", "value": "十一月去云南",
                    "kind": "goal_signal", "confidence": 70
                }
            ])))
            .await
            .expect("execute");
        let decisions = decisions(&result);
        assert_eq!(decisions.len(), 3, "one decision per input observation");
        assert_eq!(decisions[0]["decision"], "queued");
        assert_eq!(decisions[0]["key"], "editor");
        assert_eq!(decisions[1]["decision"], "recorded", "correction writes");
        assert_eq!(decisions[2]["decision"], "queued");

        // The profile holds ONLY the correction; inference stayed out.
        let profile = service.current().expect("load").state.expect("profile");
        assert!(profile.fact(SectionKind::Preferences, "editor").is_none());
        assert!(profile.fact(SectionKind::Goals, "trip").is_none());
        assert_eq!(
            profile
                .fact(SectionKind::Preferences, "tone")
                .expect("correction written")
                .value,
            "直接点"
        );

        // The queue holds the two inferred observations in input order.
        let pending = queue.load_from(0).expect("queue load");
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].1.section, "preferences");
        assert_eq!(pending[0].1.key, "editor");
        assert_eq!(pending[0].1.confidence, 80);
        assert!(!pending[0].1.enqueued_at.is_empty());
        assert_eq!(pending[1].1.section, "goals");
        assert_eq!(pending[1].1.key, "trip");
    }

    #[tokio::test]
    async fn with_a_queue_an_inference_only_batch_commits_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = service(dir.path());
        let queue = Arc::new(PendingQueue::new(pending_observations_path(dir.path())));
        let tool = ProfileUpdateTool::new(Arc::clone(&service)).with_pending_queue(queue);
        let result = tool
            .execute(args(json!([{
                "section": "environment", "key": "os", "value": "Windows",
                "kind": "stable_fact", "confidence": 90
            }])))
            .await
            .expect("execute");
        let decisions = decisions(&result);
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0]["decision"], "queued");
        assert_eq!(
            result.output.data().expect("structured")["committed"],
            json!(false)
        );
        assert!(service.current().expect("load").state.is_none());
    }
}
