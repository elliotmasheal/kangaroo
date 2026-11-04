//! `kangaroo serve` — the WebUI host (seam A/B consumer).
//!
//! Starts the upstream gateway unchanged, with the product's own capability
//! sources installed through the host seam: provider/memory/observer/tools
//! replaced per connection, and the end-of-turn WS consolidation handle owned
//! by the product's projection adapter. The gateway's routes, WebSocket
//! protocol and pairing flow are the upstream ones; there is no parallel
//! server here.

use anyhow::{Context, Result};
use chrono::Local;
use std::path::Path;
use std::sync::Arc;

use zeroclaw_api::memory_traits::Memory;
use zeroclaw_runtime::composition::RuntimeCapabilities;
use zeroclaw_super_agent::application::profile_service::ProfileService;
use zeroclaw_super_agent::application::self_service::SelfService;
use zeroclaw_super_agent::infra::host::with_supplied_memory;
use zeroclaw_super_agent::infra::paths;
use zeroclaw_super_agent::infra::profile_memory::ProfileMemoryAdapter;
use zeroclaw_super_agent::infra::transcript::{
    TranscriptObserver, TranscriptWriter, transcripts_dir,
};
use zeroclaw_super_agent::tools::ProfileToolsSource;

pub async fn run(
    root: &Path,
    mut config: zeroclaw_config::schema::Config,
    self_service: Arc<SelfService>,
    profile_service: Arc<ProfileService>,
) -> Result<()> {
    let agent_alias = "kangaroo";
    crate::assemble_product_agent(&mut config, agent_alias);

    // One product journal for the served session: the same append-only
    // transcript layer the CLI writes, with `web-` session ids so both entry
    // forms stay distinguishable in one daily file.
    let model_ref = config
        .agents
        .get(agent_alias)
        .map(|agent| agent.model_provider.to_string());
    let session_id = format!("web-{}", Local::now().format("%Y%m%d-%H%M%S"));
    let transcript = Arc::new(TranscriptWriter::new(
        transcripts_dir(root),
        session_id,
        model_ref,
    ));
    let observer = Arc::new(TranscriptObserver::new(Arc::clone(&transcript)));
    let mut capabilities = RuntimeCapabilities::config_backed_with_observer(observer);
    capabilities.tools = Arc::new(ProfileToolsSource::wrapping(
        Arc::clone(&profile_service),
        zeroclaw_runtime::composition::defaults::native_compat_registry,
    ));
    let real_memory = Arc::from(zeroclaw_memory::create_memory(
        &config.memory,
        &root.join("memory"),
        None,
    )?) as Arc<dyn Memory>;
    let adapter = Arc::new(ProfileMemoryAdapter::new(
        real_memory,
        Arc::clone(&self_service),
        Arc::clone(&profile_service),
    ));
    // The WS agent's memory source is the projection adapter, so a WebUI turn
    // reads and writes exactly the same two cognition documents as a CLI turn.
    let capabilities =
        with_supplied_memory(capabilities, Arc::clone(&adapter) as Arc<dyn Memory>);
    let consolidation_memory: Arc<dyn Memory> = adapter.clone();

    // Resolved per connection, never snapshotted at startup: the provider
    // clones the generation's capability set, and the consolidation handle is
    // the product's adapter (never a config-backed store built behind it).
    let provider: zeroclaw_gateway::TurnCapabilityProvider = Arc::new(move || {
        Some(zeroclaw_gateway::HostTurnCapabilities {
            capabilities: capabilities.clone(),
            consolidation_memory: Some(Arc::clone(&consolidation_memory)),
        })
    });

    // The product's own port, never the installed ZeroClaw's 42617.
    let port = paths::DEFAULT_GATEWAY_PORT;
    config.gateway.port = port;
    let host = config.gateway.host.clone();
    let authority = zeroclaw_runtime::LiveConfigAuthority::new_owned(config.clone())
        .context("serve: live config authority")?;
    println!(
        "{}",
        zeroclaw_runtime::i18n::get_required_cli_string_with_args(
            "cli-kangaroo-serve-url",
            &[("url", &format!("http://{host}:{port}/"))]
        )
    );

    zeroclaw_gateway::run_gateway_with_authority_and_turn_capabilities(
        &host,
        port,
        config,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        authority,
        provider,
    )
    .await
}
