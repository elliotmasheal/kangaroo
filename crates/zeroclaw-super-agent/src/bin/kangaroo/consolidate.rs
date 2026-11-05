//! The `kangaroo consolidate` adapter (memory design, Phase B): a thin
//! trigger surface for the shared consolidation service. Every bit of
//! orchestration — locking, windowing, the LLM budget, cursor writes —
//! lives in `infra::consolidation::run_pass`, so the daemon scheduler (the
//! product's target form) and the startup auto-trigger reuse one
//! implementation; this CLI check grows no exclusive business logic.

use anyhow::Result;
use std::path::Path;

use zeroclaw_super_agent::infra::consolidation::{MAX_LLM_CALLS_PER_PASS, run_pass};

fn fluent(key: &str) -> String {
    zeroclaw_runtime::i18n::get_required_cli_string(key)
}

fn fluent_args(key: &str, args: &[(&str, &str)]) -> String {
    zeroclaw_runtime::i18n::get_required_cli_string_with_args(key, args)
}

/// Render one shared consolidation pass. Manual entry point (`kangaroo
/// consolidate`) for verification and external callers. Builds its own
/// ProfileService over the same root: the promotion stage at the end of the
/// pass writes through the profile's own adjudication.
pub async fn run(root: &Path, config: &zeroclaw_config::schema::Config) -> Result<()> {
    let profile_store = zeroclaw_super_agent::infra::markdown_profile_store(root)?;
    let clock = zeroclaw_super_agent::infra::system_clock();
    let profile_service = zeroclaw_super_agent::application::profile_service::ProfileService::new(
        profile_store,
        clock,
    );
    let summary = run_pass(root, config, &profile_service).await?;
    if summary.already_running {
        println!("{}", fluent("cli-consolidate-already-running"));
        return Ok(());
    }
    println!(
        "{}",
        fluent_args(
            "cli-consolidate-summary",
            &[
                ("windows", &summary.windows.to_string()),
                ("llm_calls", &summary.llm_calls.to_string()),
                ("episodes", &summary.episodes.to_string()),
                ("hypotheses", &summary.hypotheses.to_string()),
                ("audited", &summary.audited.to_string()),
                ("not_worth", &summary.filtered_not_worth.to_string()),
                ("cursor_file", &summary.cursor.file),
                ("cursor_offset", &summary.cursor.line_offset.to_string()),
            ]
        )
    );
    if summary.llm_calls >= MAX_LLM_CALLS_PER_PASS {
        println!("{}", fluent("cli-consolidate-budget-limited"));
    }
    Ok(())
}
