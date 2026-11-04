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
use std::sync::{Arc, Mutex};

use zeroclaw_api::memory_traits::Memory;
use zeroclaw_runtime::composition::RuntimeCapabilities;
use zeroclaw_super_agent::application::orchestration::{AssignmentBoard, Router, watchdog};
use zeroclaw_super_agent::application::ports::Clock;
use zeroclaw_super_agent::application::profile_service::ProfileService;
use zeroclaw_super_agent::application::self_service::SelfService;
use zeroclaw_super_agent::domain::orchestration::DEFAULT_WATCHDOG_THRESHOLD_MS;
use zeroclaw_super_agent::infra::host::with_supplied_memory;
use zeroclaw_super_agent::infra::paths;
use zeroclaw_super_agent::infra::profile_memory::ProfileMemoryAdapter;
use zeroclaw_super_agent::infra::transcript::{
    TranscriptObserver, TranscriptWriter, transcripts_dir,
};
use zeroclaw_super_agent::tools::ProfileToolsSource;

/// Seam C consumer: the per-turn work the CLI loop does in-process — journal
/// the user/assistant lines, route the decision, supervise the assignment and
/// tick the watchdog at turn boundaries. One instance per WS connection (the
/// gateway resolves host capabilities per connection); the journal and the
/// board are shared across the served process, while the active assignment
/// belongs to the connection.
struct WebTurnLifecycle {
    journal: Arc<TranscriptWriter>,
    board: Arc<AssignmentBoard>,
    router: Router,
    clock: Arc<dyn Clock>,
    active: Mutex<Option<String>>,
}

impl WebTurnLifecycle {
    fn new(
        journal: Arc<TranscriptWriter>,
        board: Arc<AssignmentBoard>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            journal,
            board,
            router: Router::new(),
            clock,
            active: Mutex::new(None),
        }
    }

    /// Turn-boundary watchdog (the CLI's Trigger form). A served process also
    /// gets the interval form from `daemon`, but the boundary tick keeps
    /// "previous turn opened an assignment that never settled" visible even
    /// when the interval has not fired yet.
    fn run_watchdog(&self) {
        let minutes = (DEFAULT_WATCHDOG_THRESHOLD_MS / 60_000).to_string();
        for assignment in watchdog::tick_with_default_threshold(&self.board, self.clock.now_ms()) {
            println!(
                "{}",
                zeroclaw_runtime::i18n::get_required_cli_string_with_args(
                    "cli-assignment-attention",
                    &[("minutes", &minutes), ("id", &assignment.id)],
                )
            );
        }
    }

    fn lock_active(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl zeroclaw_gateway::TurnLifecycle for WebTurnLifecycle {
    fn turn_started(&self, user_text: &str) {
        // The journal keeps the user's own words; dispatch shaping of the
        // agent prompt stays CLI-only until a system-prompt seam exists
        // (pending decision #27).
        self.journal.record_user(user_text);
        self.run_watchdog();
        let ticket = self.router.route(user_text);
        let assignment = self.board.dispatch(&ticket, self.clock.now_ms());
        zeroclaw_log::record!(
            INFO,
            zeroclaw_log::Event::new(module_path!(), zeroclaw_log::Action::Note).with_attrs(
                serde_json::json!({
                    "intent": ticket.intent.as_str(),
                    "assignment": assignment,
                })
            ),
            "web turn routed"
        );
        *self.lock_active() = assignment;
    }

    fn turn_finished(&self, _user_text: &str, reply: Option<&str>) {
        let anchor = reply.and_then(|text| self.journal.record_assistant(text));
        if let Some(id) = self.lock_active().take() {
            self.board.settle(&id, anchor);
        }
    }
}

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
    let capabilities = with_supplied_memory(capabilities, Arc::clone(&adapter) as Arc<dyn Memory>);
    let consolidation_memory: Arc<dyn Memory> = adapter.clone();

    // Resolved per connection, never snapshotted at startup: the provider
    // clones the generation's capability set, and the consolidation handle is
    // the product's adapter (never a config-backed store built behind it).
    // Seam C: each connection gets its own lifecycle (per-connection active
    // assignment) over the shared journal and supervision board.
    let board = Arc::new(AssignmentBoard::new());
    let clock = zeroclaw_super_agent::infra::system_clock();
    let provider: zeroclaw_gateway::TurnCapabilityProvider = {
        let journal = Arc::clone(&transcript);
        let board = Arc::clone(&board);
        let clock = Arc::clone(&clock);
        Arc::new(move || {
            let lifecycle: Arc<dyn zeroclaw_gateway::TurnLifecycle> = Arc::new(
                WebTurnLifecycle::new(Arc::clone(&journal), Arc::clone(&board), Arc::clone(&clock)),
            );
            Some(zeroclaw_gateway::HostTurnCapabilities {
                capabilities: capabilities.clone(),
                consolidation_memory: Some(Arc::clone(&consolidation_memory)),
                turn_lifecycle: Some(lifecycle),
            })
        })
    };

    // Long-running host: the consolidation gate is re-checked on an interval
    // instead of waiting for the next manual launch (D1). This starts with one
    // catch-up attempt; the gateway loop below is never blocked by a pass.
    crate::daemon::start_scheduler(root.to_path_buf(), config.clone());

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
        &host, port, config, None, None, None, None, None, None, None, None, None, authority,
        provider,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_gateway::TurnLifecycle as _;

    #[test]
    fn a_worker_turn_is_journaled_and_its_assignment_settles() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Arc::new(TranscriptWriter::new(
            dir.path().to_path_buf(),
            "web-test".to_owned(),
            None,
        ));
        let board = Arc::new(AssignmentBoard::new());
        let clock = zeroclaw_super_agent::infra::system_clock();
        let lifecycle =
            WebTurnLifecycle::new(Arc::clone(&journal), Arc::clone(&board), Arc::clone(&clock));

        lifecycle.turn_started("帮我实现这个功能");
        assert_eq!(board.len(), 1, "a delegate ticket opens an assignment");
        lifecycle.turn_finished("帮我实现这个功能", Some("已完成，证据是 src/lib.rs。"));

        // The assignment is settled, so the watchdog has nothing to raise.
        let now = clock.now_ms();
        assert!(
            board
                .due_attention(
                    now + DEFAULT_WATCHDOG_THRESHOLD_MS,
                    DEFAULT_WATCHDOG_THRESHOLD_MS,
                )
                .is_empty(),
            "a finished turn leaves no stalled assignment"
        );

        // The journal holds the user's own words and the assistant reply.
        let day = chrono::Local::now().format("%Y-%m-%d").to_string();
        let raw =
            std::fs::read_to_string(dir.path().join(format!("{day}.jsonl"))).expect("journal file");
        let lines: Vec<serde_json::Value> = raw
            .lines()
            .map(|line| serde_json::from_str(line).expect("journal json"))
            .collect();
        assert_eq!(lines.len(), 2, "user + assistant: {raw}");
        assert_eq!(lines[0]["role"], "user");
        assert_eq!(
            lines[0]["text"], "帮我实现这个功能",
            "the journal keeps the raw words, never the dispatch directive"
        );
        assert_eq!(lines[1]["role"], "assistant");
        assert_eq!(lines[1]["text"], "已完成，证据是 src/lib.rs。");
    }

    #[test]
    fn an_inline_turn_opens_no_assignment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Arc::new(TranscriptWriter::new(
            dir.path().to_path_buf(),
            "web-test".to_owned(),
            None,
        ));
        let board = Arc::new(AssignmentBoard::new());
        let clock = zeroclaw_super_agent::infra::system_clock();
        let lifecycle = WebTurnLifecycle::new(journal, Arc::clone(&board), clock);
        lifecycle.turn_started("你好");
        assert!(board.is_empty(), "inline turns have no lifecycle");
        lifecycle.turn_finished("你好", Some("你好，有什么事直接说就行。"));
    }
}
