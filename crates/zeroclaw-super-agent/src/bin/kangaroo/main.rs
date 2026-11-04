//! 袋鼠 (Kangaroo) — the product binary's minimal runnable loop (wiring
//! milestone).
//!
//! Startup isolation: the runtime bootstrap env points at the product's own
//! `~/.kangaroo` directory, never an installed ZeroClaw's `~/.zeroclaw`; the
//! gateway default port is 42618, never 42617. First run walks the
//! questionnaire adapter (outer layer — see `intake.rs`); afterwards each
//! stdin line is one agent turn with the dual projection injected through
//! the memory adapter and the profile tools supplied through the ToolSource
//! seam. No `zeroclaw-runtime` source file is involved.

mod intake;

use anyhow::{Context, Result, bail};
use std::io::{BufRead, Write};
use std::sync::Arc;

use zeroclaw_runtime::agent::loop_::AgentRunOverrides;
use zeroclaw_runtime::composition::RuntimeCapabilities;
use zeroclaw_super_agent::application::profile_service::ProfileService;
use zeroclaw_super_agent::application::self_service::SelfService;
use zeroclaw_super_agent::infra::paths;
use zeroclaw_super_agent::infra::profile_memory::ProfileMemoryAdapter;
use zeroclaw_super_agent::tools::ProfileToolsSource;

/// The product's agent contract in the canonical config: the `kangaroo`
/// alias must exist and name a risk profile the turn resolver can find.
/// Fresh installs carry neither section; both are assembled in memory (the
/// saved-config RFC will pin them later). Every entry point runs this bin,
/// so a bare exe, the launcher, and a future WebUI launch all get the same
/// defaults without depending on any script-injected state.
fn assemble_product_agent(config: &mut zeroclaw_config::schema::Config, agent_alias: &str) {
    let agent = config.agents.entry(agent_alias.to_owned()).or_default();
    // An alias present on disk may still carry the empty ("no preference")
    // ref the schema default uses; turn resolution requires a named entry,
    // so the ref is pointed at the profile assembled alongside it. A config
    // that exists but lacks fields is not a reason to fail the turn.
    agent.risk_profile = zeroclaw_config::providers::RiskProfileRef::new(agent_alias);
    config
        .risk_profiles
        .entry(agent_alias.to_owned())
        .or_default();
}

fn prompt(text: &str) {
    print!("{text}");
    let _ = std::io::stdout().flush();
}

fn cli(key: &str) -> String {
    zeroclaw_runtime::i18n::get_required_cli_string(key)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Isolation at the startup path: the product's own data root.
    let root = paths::default_data_root()?;
    if !paths::isolation_holds() {
        bail!("data directory isolation failed: refusing to touch an installed ZeroClaw's state");
    }
    std::fs::create_dir_all(&root)
        .with_context(|| format!("failed to create {}", root.display()))?;
    // SAFETY: single-threaded startup before any runtime config read; this
    // points the runtime's bootstrap env at ~/.kangaroo so no code path can
    // reach ~/.zeroclaw by default.
    unsafe {
        std::env::set_var("ZEROCLAW_CONFIG_DIR", root.join("config"));
        std::env::set_var("ZEROCLAW_DATA_DIR", root.join("data"));
    }
    println!(
        "{}",
        zeroclaw_runtime::i18n::get_required_cli_string_with_args(
            "cli-kangaroo-starting",
            &[
                ("data_dir", &root.display().to_string()),
                ("port", &paths::DEFAULT_GATEWAY_PORT.to_string()),
            ]
        )
    );

    let mut config = zeroclaw_config::schema::Config::load_or_init().await?;
    // Hard constraint: the resolved config may only address ~/.kangaroo; any
    // surviving .zeroclaw reference (bootstrap default, env, operator entry)
    // fails startup instead of falling back to ZeroClaw state.
    paths::assert_startup_isolation(&config, &root)?;
    let self_store = zeroclaw_super_agent::infra::markdown_self_store(&root)?;
    let profile_store = zeroclaw_super_agent::infra::markdown_profile_store(&root)?;
    let negotiation_log = zeroclaw_super_agent::infra::jsonl_negotiation_log(&root)?;
    let clock = zeroclaw_super_agent::infra::system_clock();
    let self_service = Arc::new(SelfService::new(
        Arc::clone(&self_store),
        negotiation_log,
        Arc::clone(&clock),
        Arc::new(zeroclaw_super_agent::domain::selfhood::negotiation::ValidationPolicy),
    ));
    let profile_service = Arc::new(ProfileService::new(profile_store, clock));

    // First run: the questionnaire adapter assembles the draft and commits
    // it through the core use case. An abandoned questionnaire writes
    // nothing and restarts on the next launch.
    if self_service.current()?.state.is_none() {
        println!("{}", cli("cli-intake-opening"));
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        let draft = intake::run_questionnaire(&stdin, &mut stdout)?;
        let revision = intake::commit_draft(draft, &self_service, &profile_service)?;
        println!(
            "{}",
            zeroclaw_runtime::i18n::get_required_cli_string_with_args(
                "cli-intake-done",
                &[("revision", &revision.to_string())]
            )
        );
    }

    // Capability generation: config-backed sources, the profile tools
    // wrapped onto the native registry via the ToolSource seam, and the real
    // memory backend wrapped in the projection adapter.
    let observer = Arc::new(zeroclaw_runtime::observability::NoopObserver);
    let mut capabilities = RuntimeCapabilities::config_backed_with_observer(observer);
    capabilities.tools = Arc::new(ProfileToolsSource::wrapping(
        Arc::clone(&profile_service),
        zeroclaw_runtime::composition::defaults::native_compat_registry,
    ));
    let real_memory: Arc<dyn zeroclaw_memory::Memory> = Arc::from(zeroclaw_memory::create_memory(
        &config.memory,
        &root.join("memory"),
        None,
    )?);
    let adapter = Arc::new(ProfileMemoryAdapter::new(
        real_memory,
        Arc::clone(&self_service),
        Arc::clone(&profile_service),
    ));

    // The agent alias must exist in the canonical config (`agents` map);
    // see `assemble_product_agent` for why this happens in the bin itself.
    let agent_alias = "kangaroo";
    assemble_product_agent(&mut config, agent_alias);
    println!("{}", cli("cli-kangaroo-chat-ready"));
    let stdin = std::io::stdin();
    loop {
        prompt("\n> ");
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let message = line.trim().to_owned();
        if message.is_empty() {
            continue;
        }
        if message == ":quit" {
            break;
        }
        let overrides = AgentRunOverrides {
            memory: Some(Arc::clone(&adapter) as Arc<dyn zeroclaw_memory::Memory>),
            suppress_memory_auto_save: true,
            ..Default::default()
        };
        match zeroclaw_super_agent::begin_turn_turn(
            config.clone(),
            capabilities.clone(),
            agent_alias,
            message,
            overrides,
        )
        .await
        {
            // The runtime's CLI channel already streams the response text to
            // stdout; printing it here duplicated every reply.
            Ok(_) => {}
            Err(error) => println!("error: {error:#}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bare-exe regression: a config without any `[agents.*]` or
    /// `risk_profiles` section (what a fresh install loads) must still admit
    /// a turn after the in-memory assembly, without script-injected state.
    #[test]
    fn a_minimal_config_gets_the_product_agent_and_named_risk_profile() {
        let mut config = zeroclaw_config::schema::Config::default();
        assert!(!config.agents.contains_key("kangaroo"));
        assert!(config.risk_profiles.is_empty());
        assemble_product_agent(&mut config, "kangaroo");
        let agent = config.agents.get("kangaroo").expect("agent assembled");
        assert_eq!(agent.risk_profile.as_str(), "kangaroo");
        assert!(config.risk_profiles.contains_key("kangaroo"));
    }

    /// A config that exists on disk but predates a named risk_profile ref
    /// (empty "no preference" ref) must be upgraded in memory, not fail.
    #[test]
    fn an_existing_agent_entry_is_upgraded_to_a_named_risk_profile() {
        let mut config = zeroclaw_config::schema::Config::default();
        let agent = zeroclaw_config::schema::AliasedAgentConfig {
            risk_profile: zeroclaw_config::providers::RiskProfileRef::new(""),
            ..Default::default()
        };
        config.agents.insert("kangaroo".to_owned(), agent);
        assemble_product_agent(&mut config, "kangaroo");
        assert_eq!(config.agents["kangaroo"].risk_profile.as_str(), "kangaroo");
        assert!(config.risk_profiles.contains_key("kangaroo"));
    }
}
