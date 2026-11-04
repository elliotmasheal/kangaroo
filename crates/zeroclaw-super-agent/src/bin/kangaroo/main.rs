//! 袋鼠 (Kangaroo) — the product binary's minimal runnable loop (wiring
//! milestone).
//!
//! Startup isolation: the runtime bootstrap env points at the product's own
//! `~/.kangaroo` directory, never an installed ZeroClaw's `~/.zeroclaw`; the
//! gateway default port is 42618, never 42617. First run walks the
//! questionnaire adapter (outer layer — see `intake.rs`); afterwards each
//! stdin line is one agent turn with the dual projection injected through
//! the memory adapter and the profile tools supplied through the ToolSource
//! seam. The only runtime edits are the two additive host seams (the
//! supplied-capabilities constructor and the gateway's turn-capability
//! provider); no parallel gateway or agent rewrite.

mod consolidate;
mod daemon;
mod intake;
mod serve;

use anyhow::{Context, Result, bail};
use chrono::Local;
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use zeroclaw_api::observability_traits::{Observer, ObserverEvent, ObserverMetric};
use zeroclaw_runtime::agent::loop_::AgentRunOverrides;
use zeroclaw_runtime::composition::RuntimeCapabilities;
use zeroclaw_super_agent::application::orchestration::{
    AssignmentBoard, Router, dispatch_directive, watchdog,
};
use zeroclaw_super_agent::application::profile_service::ProfileService;
use zeroclaw_super_agent::application::self_service::SelfService;
use zeroclaw_super_agent::domain::orchestration::DEFAULT_WATCHDOG_THRESHOLD_MS;
use zeroclaw_super_agent::infra::paths;
use zeroclaw_super_agent::infra::profile_memory::ProfileMemoryAdapter;
use zeroclaw_super_agent::infra::transcript::{
    TranscriptObserver, TranscriptWriter, transcripts_dir,
};
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

/// What the interactive loop does with one stdin line. The loop's ONLY
/// input source is stdin — a different handle than the stdout the runtime
/// streams replies to — so assistant output can never re-enter as user
/// input through this classifier (the 2026-11-04 self-talk incident was
/// console noise from a shredded launcher entering stdin, not a role mix).
#[derive(Debug)]
enum InputAction {
    Skip,
    Quit,
    Message(String),
}

fn classify_input(line: &str) -> InputAction {
    let message = line.trim().to_owned();
    if message.is_empty() {
        InputAction::Skip
    } else if message == ":quit" {
        InputAction::Quit
    } else {
        InputAction::Message(message)
    }
}

fn prompt(text: &str) {
    print!("{text}");
    let _ = std::io::stdout().flush();
}

fn cli(key: &str) -> String {
    zeroclaw_runtime::i18n::get_required_cli_string(key)
}

/// Bridges the runtime's tool-call events to the assignment board: the first
/// worker execution observed in the active turn moves its assignment from
/// `Dispatched` to `Running`. The transcript observer still receives every
/// event unchanged (it journals completed tool calls).
struct WorkerProbe {
    journal: TranscriptObserver,
    active: Mutex<Option<String>>,
    board: Arc<AssignmentBoard>,
}

impl WorkerProbe {
    fn new(journal: TranscriptObserver, board: Arc<AssignmentBoard>) -> Self {
        Self {
            journal,
            active: Mutex::new(None),
            board,
        }
    }

    /// Names the assignment the turn about to run belongs to; `None` clears.
    fn set_active(&self, assignment: Option<String>) {
        if let Ok(mut slot) = self.active.lock() {
            *slot = assignment;
        }
    }
}

impl Observer for WorkerProbe {
    fn record_event(&self, event: &ObserverEvent) {
        if let ObserverEvent::ToolCallStart { tool, .. } = event
            && matches!(tool.as_str(), "spawn_subagent" | "delegate")
            && let Some(id) = self.active.lock().ok().and_then(|slot| slot.clone())
        {
            self.board.mark_running(&id);
        }
        self.journal.record_event(event);
    }

    fn record_metric(&self, metric: &ObserverMetric) {
        self.journal.record_metric(metric);
    }

    fn name(&self) -> &str {
        self.journal.name()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
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
    if std::env::args().nth(1).as_deref() == Some("consolidate") {
        // Phase B: nightly consolidation pass (scheduled externally).
        return consolidate::run(&root, &config).await;
    }
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
    let profile_service = Arc::new(ProfileService::new(profile_store, Arc::clone(&clock)));

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

    // WebUI entry: the upstream gateway with the product's supplied
    // capabilities. It reuses the services and the intake state above, so a
    // first run completes the questionnaire before the server starts.
    if std::env::args().nth(1).as_deref() == Some("serve") {
        return serve::run(&root, config, self_service, profile_service).await;
    }

    // Capability generation: config-backed sources, the profile tools
    // wrapped onto the native registry via the ToolSource seam, the real
    // memory backend wrapped in the projection adapter, and the append-only
    // conversation transcript (journal layer — never brain.db, never blocks).
    let agent_alias = "kangaroo";
    let model_ref = config
        .agents
        .get(agent_alias)
        .map(|agent| agent.model_provider.to_string());
    let session_id = format!("cli-{}", Local::now().format("%Y%m%d-%H%M%S"));
    let transcript = Arc::new(TranscriptWriter::new(
        transcripts_dir(&root),
        session_id,
        model_ref,
    ));
    let board = Arc::new(AssignmentBoard::new());
    let probe = Arc::new(WorkerProbe::new(
        TranscriptObserver::new(Arc::clone(&transcript)),
        Arc::clone(&board),
    ));
    let mut capabilities = RuntimeCapabilities::config_backed_with_observer(probe.clone());
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
    assemble_product_agent(&mut config, agent_alias);

    // Daemon-semantics consolidation trigger (memory design, Phase B): on
    // startup, a stale last-success plus pending journal lines starts ONE
    // background pass. The conversation loop is never blocked; a failure is
    // logged and left for the next launch (no in-session retry). The gate
    // and the pass live in `daemon`, shared with the long-running host.
    daemon::start_startup_catch_up(root.clone(), config.clone());

    // M3 orchestration (minimal closure): the deterministic Router opens the
    // decision, the in-memory board supervises worker-bound turns, and the
    // watchdog runs at each turn boundary. Worker execution itself stays on
    // the runtime's spawn_subagent/delegate tools — this layer only routes,
    // records, and reports.
    let router = Router::new();
    let watchdog_minutes = (DEFAULT_WATCHDOG_THRESHOLD_MS / 60_000).to_string();
    println!("{}", cli("cli-kangaroo-chat-ready"));
    let stdin = std::io::stdin();
    loop {
        // Trigger (turn-boundary form until the daemon owns an interval):
        // report once per stalled assignment, never silently retry it.
        for assignment in watchdog::tick_with_default_threshold(&board, clock.now_ms()) {
            println!(
                "{}",
                zeroclaw_runtime::i18n::get_required_cli_string_with_args(
                    "cli-assignment-attention",
                    &[("minutes", &watchdog_minutes), ("id", &assignment.id),]
                )
            );
        }
        prompt("\n> ");
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        match classify_input(&line) {
            InputAction::Skip => continue,
            InputAction::Quit => break,
            InputAction::Message(message) => {
                transcript.record_user(&message);
                let ticket = router.route(&message);
                let assignment = board.dispatch(&ticket, clock.now_ms());
                probe.set_active(assignment.clone());
                // Outbox class `acknowledgement`: a worker-bound turn
                // confirms receipt before the (slower) worker work starts.
                if ticket.intent.needs_worker() {
                    println!("{}", cli("cli-turn-acknowledged"));
                }
                // The journal keeps the user's own words; the dispatch
                // directive only shapes the turn the agent runs.
                let turn_message = match dispatch_directive(&ticket) {
                    Some(directive) => format!("{directive}\n\n{message}"),
                    None => message,
                };
                let overrides = AgentRunOverrides {
                    memory: Some(Arc::clone(&adapter) as Arc<dyn zeroclaw_memory::Memory>),
                    suppress_memory_auto_save: true,
                    ..Default::default()
                };
                match zeroclaw_super_agent::begin_turn_turn(
                    config.clone(),
                    capabilities.clone(),
                    agent_alias,
                    turn_message,
                    overrides,
                )
                .await
                {
                    // The runtime's CLI channel already streams the response
                    // text to stdout; the full reply is journaled here and its
                    // trace anchor settles the assignment.
                    Ok(response) => {
                        let anchor = transcript.record_assistant(&response);
                        if let Some(id) = assignment {
                            board.settle(&id, anchor);
                        }
                    }
                    Err(error) => {
                        println!("error: {error:#}");
                        if let Some(id) = assignment {
                            board.settle(&id, None);
                        }
                    }
                }
                probe.set_active(None);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_super_agent::domain::orchestration::{AssignmentStatus, Ticket, TicketIntent};

    fn worker_tool_event(tool: &str) -> ObserverEvent {
        ObserverEvent::ToolCallStart {
            tool: tool.to_owned(),
            tool_call_id: None,
            arguments: None,
            channel: None,
            agent_alias: None,
            parent_agent_alias: None,
            turn_id: None,
        }
    }

    fn board_with_delegate_ticket(
        dir: &std::path::Path,
    ) -> (Arc<AssignmentBoard>, Arc<WorkerProbe>, String) {
        let writer = Arc::new(TranscriptWriter::new(
            dir.to_path_buf(),
            "test-session".to_owned(),
            None,
        ));
        let board = Arc::new(AssignmentBoard::new());
        let probe = Arc::new(WorkerProbe::new(
            TranscriptObserver::new(writer),
            Arc::clone(&board),
        ));
        let id = board
            .dispatch(
                &Ticket {
                    intent: TicketIntent::Delegate,
                    matched_phrase: None,
                },
                0,
            )
            .expect("worker ticket opens an assignment");
        (board, probe, id)
    }

    /// The board transition the outline names: an observed worker tool call
    /// moves the active assignment `Dispatched → Running`.
    #[test]
    fn an_observed_worker_tool_moves_the_assignment_to_running() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (board, probe, id) = board_with_delegate_ticket(dir.path());
        probe.set_active(Some(id.clone()));
        probe.record_event(&worker_tool_event("spawn_subagent"));
        assert_eq!(
            board.get(&id).expect("assignment present").status,
            AssignmentStatus::Running
        );
    }

    /// Only worker tools transition, and only for the active turn.
    #[test]
    fn non_worker_tools_and_idle_turns_leave_the_assignment_dispatched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (board, probe, id) = board_with_delegate_ticket(dir.path());
        probe.set_active(Some(id.clone()));
        probe.record_event(&worker_tool_event("shell"));
        assert_eq!(
            board.get(&id).expect("assignment present").status,
            AssignmentStatus::Dispatched,
            "a non-worker tool is not an assignment start"
        );
        probe.record_event(&worker_tool_event("spawn_subagent"));
        assert_eq!(
            board.get(&id).expect("assignment present").status,
            AssignmentStatus::Running
        );
        probe.set_active(None);
    }

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

    /// The interactive loop's input contract: every non-empty, non-control
    /// stdin line is ONE user message. Assistant replies live on stdout —
    /// a handle this classifier never touches — so their text can only
    /// become a user turn if something outside this process writes it to
    /// the console input buffer (the shredded-launcher incident).
    #[test]
    fn stdin_classification_keeps_the_user_channel_structural() {
        assert!(matches!(classify_input("\n"), InputAction::Skip));
        assert!(matches!(classify_input("   \n"), InputAction::Skip));
        assert!(matches!(classify_input(":quit\n"), InputAction::Quit));
        assert!(matches!(classify_input("  :quit  \n"), InputAction::Quit));
        match classify_input("你好\n") {
            InputAction::Message(message) => assert_eq!(message, "你好"),
            other => panic!("expected a message, got {other:?}"),
        }
    }
}
