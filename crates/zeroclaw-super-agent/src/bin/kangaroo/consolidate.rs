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
/// consolidate`) for verification and external callers.
pub async fn run(root: &Path, config: &zeroclaw_config::schema::Config) -> Result<()> {
    let summary = run_pass(root, config).await?;
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
