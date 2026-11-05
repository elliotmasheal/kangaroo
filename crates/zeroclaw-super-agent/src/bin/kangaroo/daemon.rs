//! Daemon-semantics scheduling shared by the interactive bin and `serve`
//! (Phase B / D1): one consolidation gate, two trigger shapes. The startup
//! check is the CLI's catch-up; `serve` additionally re-checks on an interval
//! so a long-running host does not wait for the next manual launch. Both call
//! the same `consolidation::run_pass` — this module owns only the timing.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use zeroclaw_config::schema::Config;
use zeroclaw_log::{Action, Event, EventOutcome, record};
use zeroclaw_super_agent::application::profile_service::ProfileService;
use zeroclaw_super_agent::infra::consolidation;
use zeroclaw_super_agent::infra::episodes;

/// How often a long-running host re-checks the consolidation gate. One hour
/// is a cheap poll; the gate itself (`auto_trigger_due`, 20h staleness) is
/// what decides whether a pass actually runs.
pub const SCHEDULER_CHECK_MINUTES: u64 = 60;

fn due(root: &Path) -> anyhow::Result<bool> {
    let cursor = episodes::load_cursor(&episodes::cursor_path(root))?.unwrap_or_default();
    consolidation::auto_trigger_due(root, &cursor)
}

/// The CLI's startup catch-up: exactly one attempt when the gate is open.
/// When the gate is closed nothing is spawned, so an idle launch stays idle.
pub fn start_startup_catch_up(root: PathBuf, config: Config, profile_service: Arc<ProfileService>) {
    match due(&root) {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            record!(
                WARN,
                Event::new(module_path!(), Action::Note)
                    .with_outcome(EventOutcome::Failure)
                    .with_attrs(serde_json::json!({ "error": format!("{error:#}") })),
                "consolidation gate could not be read; skipping startup catch-up"
            );
            return;
        }
    }
    println!("{}", crate::cli("cli-consolidate-started"));
    zeroclaw_spawn::spawn!(async move {
        run_and_report(&root, &config, &profile_service).await;
    });
}

/// The long-running host's schedule: one startup attempt, then a periodic
/// gate check. Errors are logged and left for the next tick — never retried
/// in a tight loop.
pub fn start_scheduler(root: PathBuf, config: Config, profile_service: Arc<ProfileService>) {
    zeroclaw_spawn::spawn!(async move {
        run_if_due(&root, &config, &profile_service).await;
        let mut ticker = tokio::time::interval(Duration::from_secs(SCHEDULER_CHECK_MINUTES * 60));
        // The first tick resolves immediately; the startup attempt above
        // already covered it.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            run_if_due(&root, &config, &profile_service).await;
        }
    });
}

async fn run_if_due(root: &Path, config: &Config, profile_service: &ProfileService) {
    match due(root) {
        Ok(true) => {
            println!("{}", crate::cli("cli-consolidate-started"));
            run_and_report(root, config, profile_service).await;
        }
        Ok(false) => {}
        Err(error) => record!(
            WARN,
            Event::new(module_path!(), Action::Note)
                .with_outcome(EventOutcome::Failure)
                .with_attrs(serde_json::json!({ "error": format!("{error:#}") })),
            "consolidation gate could not be read; left for the next tick"
        ),
    }
}

async fn run_and_report(root: &Path, config: &Config, profile_service: &ProfileService) {
    match consolidation::run_pass(root, config, profile_service).await {
        // Another pass holds the lock: this tick is skipped silently, the
        // next tick re-checks.
        Ok(summary) if summary.already_running => {}
        Ok(summary) => println!(
            "{}",
            zeroclaw_runtime::i18n::get_required_cli_string_with_args(
                "cli-consolidate-auto-done",
                &[
                    ("episodes", &summary.episodes.to_string()),
                    ("hypotheses", &summary.hypotheses.to_string()),
                ]
            )
        ),
        Err(error) => record!(
            WARN,
            Event::new(module_path!(), Action::Note)
                .with_outcome(EventOutcome::Failure)
                .with_attrs(serde_json::json!({ "error": format!("{error:#}") })),
            "consolidation pass failed; left for the next trigger"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scheduler_polls_well_below_the_staleness_gate() {
        // The poll interval must be far smaller than the 20h gate, otherwise
        // a long-running host would sleep past a night's worth of pending
        // lines.
        assert!(
            (SCHEDULER_CHECK_MINUTES as i64) * 60 < consolidation::AUTO_TRIGGER_MIN_HOURS * 60 * 60,
            "one-hour polling stays well under the staleness gate"
        );
    }
}
