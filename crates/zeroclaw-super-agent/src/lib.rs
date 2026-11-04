//! Independent-agent domain layer for ZeroClaw.
//!
//! Hosts two cognition documents as separate single sources of truth: the
//! Self aggregate (given facts + earned traits, experience-driven) and the
//! UserProfile aggregate (facts about the user, user-editable). Given facts
//! change only through negotiation; Earned traits change only through
//! experience-driven `SelfAmendment` behind an `AmendmentAuthority`.
//!
//! Layering: `domain` is pure (no IO), `application` orchestrates through
//! ports, `infra` persists Markdown documents with TOML front matter plus
//! JSONL revision logs. Capability wiring for the runtime agent loop arrives
//! through `zeroclaw_runtime::composition::RuntimeCapabilities` (see
//! `application::super_agent_app`).

pub mod application;
pub mod domain;
pub mod infra;
pub mod tools;

/// One real agent turn with the dual projection and profile tools wired in:
/// the runtime's capability-driven entry point, called with the product's
/// generation snapshot and the projection memory adapter supplied through
/// `AgentRunOverrides`. This is the M2 hookup seam: the CLI path needs no
/// runtime change; the WebSocket path uses the additive supplied-capabilities
/// constructor instead of a parallel agent implementation.
pub async fn begin_turn_turn(
    config: zeroclaw_config::schema::Config,
    capabilities: zeroclaw_runtime::composition::RuntimeCapabilities,
    agent_alias: &str,
    message: String,
    overrides: zeroclaw_runtime::agent::loop_::AgentRunOverrides,
) -> anyhow::Result<String> {
    zeroclaw_runtime::agent::run_with_capabilities(
        config,
        capabilities,
        None,
        agent_alias,
        Some(message),
        None,
        None,
        None,
        vec![],
        true,
        None,
        None,
        zeroclaw_api::ingress::TurnOrigin::Interactive,
        overrides,
    )
    .await
}
