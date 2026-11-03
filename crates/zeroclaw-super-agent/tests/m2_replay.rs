//! M2 replay tests: the profile tools driven through the zeroclaw-eval
//! replay mechanism (scripted LLM turns, sealed tool registry, real
//! dispatcher), graded against the real UserProfile documents on disk.
//!
//! The negotiation-reject path is a service-level use case (the individual
//! adjudicates, no tool call involved) and is covered by
//! `m1_behavior::negotiation_reject_path_is_recorded_and_changes_nothing`.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use tempfile::TempDir;
use zeroclaw_api::model_provider::ConversationMessage;
use zeroclaw_eval::case::LlmTrace;
use zeroclaw_super_agent::application::ports::Clock;
use zeroclaw_super_agent::application::profile_service::ProfileService;
use zeroclaw_super_agent::domain::profile::{Observation, ObservationKind, SectionKind};
use zeroclaw_super_agent::infra;
use zeroclaw_super_agent::tools::{ProfileRollbackTool, ProfileUpdateTool};

/// True when any tool result in the run's history contains `needle`.
fn tool_output_contains(record: &zeroclaw_eval::record::RunRecord, needle: &str) -> bool {
    record.history.iter().any(|message| match message {
        ConversationMessage::ToolResults(results) => {
            results.iter().any(|result| result.content.contains(needle))
        }
        _ => false,
    })
}

/// Number of revisions recorded for the profile document on disk.
fn revision_count(root: &Path) -> usize {
    std::fs::read_to_string(root.join("user_profile.revisions.jsonl"))
        .map(|raw| raw.lines().filter(|line| !line.trim().is_empty()).count())
        .unwrap_or(0)
}

struct FixedClock;

impl Clock for FixedClock {
    fn today(&self) -> zeroclaw_super_agent::domain::DateYmd {
        zeroclaw_super_agent::domain::DateYmd::new(2026, 11, 3).expect("valid")
    }

    fn now_ms(&self) -> u64 {
        1_000
    }
}

fn profile_service(root: &Path) -> Arc<ProfileService> {
    Arc::new(ProfileService::new(
        infra::markdown_profile_store(root).expect("store"),
        Arc::new(FixedClock),
    ))
}

/// Replays one trace through a sealed registry holding exactly the two
/// profile tools, mirroring zeroclaw-eval's `run_case` assembly with the
/// eval-supplied tool set replaced by ours.
async fn replay(
    trace: &LlmTrace,
    service: Arc<ProfileService>,
) -> Result<zeroclaw_eval::record::RunRecord> {
    let tmp = tempfile::tempdir()?;
    let mem_cfg = zeroclaw_config::schema::MemoryConfig {
        backend: "none".into(),
        ..Default::default()
    };
    let memory: Arc<dyn zeroclaw_memory::Memory> =
        Arc::from(zeroclaw_memory::create_memory(&mem_cfg, tmp.path(), None)?);
    let observer = Arc::new(zeroclaw_eval::observer::RecordingObserver::new());
    let replay_provider = zeroclaw_eval::replay::TraceLlmProvider::from_trace(trace);
    let replay_handle = replay_provider.handle();
    let provider: Box<dyn zeroclaw_api::model_provider::ModelProvider> = Box::new(replay_provider);

    let eval_config = zeroclaw_config::schema::Config::default();
    let eval_security = Arc::new(zeroclaw_config::policy::SecurityPolicy::default());
    let registry = zeroclaw_runtime::tools::scoped::ScopedToolRegistry::assemble(
        zeroclaw_runtime::tools::scoped::ScopedAssembly {
            config: &eval_config,
            agent_alias: "eval",
            security: &eval_security,
            built: zeroclaw_runtime::tools::AllToolsResult::from_prebuilt_tools(vec![
                Box::new(ProfileUpdateTool::new(Arc::clone(&service))),
                Box::new(ProfileRollbackTool::new(service)),
            ]),
            skills: &[],
            runtime: Arc::new(zeroclaw_runtime::platform::NativeRuntime::new()),
            caller_allowed: None,
            connect_mcp: false,
            connect_peripherals: false,
            exclude_memory: false,
            acp_delivery: false,
            list_deferred_mcp_specs: false,
            emit_assembly_logs: false,
            mcp_registry: None,
        },
    )
    .await
    .registry;

    let mut agent = zeroclaw_runtime::agent::agent::Agent::builder()
        .model_provider(provider)
        .tools(registry)
        .memory(memory)
        .observer(observer.clone())
        .tool_dispatcher(Box::new(
            zeroclaw_runtime::agent::dispatcher::NativeToolDispatcher,
        ))
        .workspace_dir(tmp.path().to_path_buf())
        .build()?;

    let mut final_response = String::new();
    for (turn_index, turn) in trace.turns.iter().enumerate() {
        final_response = agent.turn(&turn.user_input).await?;
        replay_handle.finish_turn(turn_index)?;
    }
    let (input_tokens, output_tokens) = observer.tokens();
    Ok(zeroclaw_eval::record::RunRecord {
        final_response,
        history: agent.history().to_vec(),
        tool_calls: observer.calls(),
        input_tokens,
        output_tokens,
    })
}

fn update_call(observations: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "model_name": "m2-profile-update",
        "turns": [{
            "user_input": "remember this about me",
            "steps": [
                { "response": { "type": "tool_calls", "tool_calls": [
                    { "id": "call_1", "name": "profile_update", "arguments": { "observations": observations } }
                ], "input_tokens": 30, "output_tokens": 15 } },
                { "response": { "type": "text", "content": "PROFILE RESULT", "input_tokens": 10, "output_tokens": 5 } }
            ]
        }]
    })
}

#[tokio::test]
async fn worth_recording_lands_in_the_profile_document() -> Result<()> {
    let tmp = TempDir::new()?;
    let service = profile_service(tmp.path());
    let trace: LlmTrace = serde_json::from_value(update_call(serde_json::json!([
        { "section": "preferences", "key": "editor", "value": "Neovim" }
    ])))?;
    let record = replay(&trace, Arc::clone(&service)).await?;
    assert!(
        tool_output_contains(&record, "recorded"),
        "tool result missing from history: {:?}",
        record.final_response
    );
    let state = service.current()?.state.expect("profile exists");
    assert_eq!(
        state
            .fact(SectionKind::Preferences, "editor")
            .expect("fact present")
            .value,
        "Neovim"
    );
    Ok(())
}

#[tokio::test]
async fn credentials_are_refused_by_the_criterion() -> Result<()> {
    let tmp = TempDir::new()?;
    let service = profile_service(tmp.path());
    let trace: LlmTrace = serde_json::from_value(update_call(serde_json::json!([
        { "section": "environment", "key": "api_key", "value": "sk-abcdef1234567890", "kind": "correction" }
    ])))?;
    let record = replay(&trace, Arc::clone(&service)).await?;
    assert!(
        tool_output_contains(&record, "credential-like content"),
        "history: {:?}",
        record.history
    );
    assert!(service.current()?.state.is_none(), "nothing written");
    Ok(())
}

#[tokio::test]
async fn rollback_restores_the_pre_trace_revision() -> Result<()> {
    let tmp = TempDir::new()?;
    let root = tmp.path().to_path_buf();
    let service = profile_service(&root);
    // Seed revision 1 before the replay.
    service.observe_batch(&[Observation {
        section: SectionKind::Goals,
        key: "focus".into(),
        value: "ship M2".into(),
        kind: ObservationKind::GoalSignal,
        confidence: 90,
    }])?;

    let trace: LlmTrace = serde_json::from_value(serde_json::json!({
        "model_name": "m2-profile-rollback",
        "turns": [{
            "user_input": "actually revert the profile",
            "steps": [
                { "response": { "type": "tool_calls", "tool_calls": [
                    { "id": "call_1", "name": "profile_rollback", "arguments": { "revision": 1 } }
                ], "input_tokens": 30, "output_tokens": 15 } },
                { "response": { "type": "text", "content": "ROLLED BACK", "input_tokens": 10, "output_tokens": 5 } }
            ]
        }]
    }))?;
    let record = replay(&trace, Arc::clone(&service)).await?;
    assert!(
        tool_output_contains(&record, "rolled_back_to"),
        "history: {:?}",
        record.history
    );

    let state = service.current()?.state.expect("profile exists");
    assert_eq!(
        state
            .fact(SectionKind::Goals, "focus")
            .expect("the pre-trace fact survives the rollback")
            .value,
        "ship M2"
    );
    let _ = root;
    Ok(())
}

#[tokio::test]
async fn user_correction_is_not_overwritten_by_agent_inference() -> Result<()> {
    let tmp = TempDir::new()?;
    let service = profile_service(tmp.path());
    // The user stated this explicitly: a Correction is the highest-priority
    // source, so later agent inference on the same key must not displace it.
    service.observe_batch(&[Observation {
        section: SectionKind::Preferences,
        key: "editor".into(),
        value: "Emacs".into(),
        kind: ObservationKind::Correction,
        confidence: 95,
    }])?;

    let trace: LlmTrace = serde_json::from_value(update_call(serde_json::json!([
        { "section": "preferences", "key": "editor", "value": "Neovim" }
    ])))?;
    let record = replay(&trace, Arc::clone(&service)).await?;
    assert!(
        tool_output_contains(&record, "kept the existing higher-priority or newer fact"),
        "history: {:?}",
        record.history
    );
    let state = service.current()?.state.expect("profile exists");
    assert_eq!(
        state
            .fact(SectionKind::Preferences, "editor")
            .expect("the user's fact survives")
            .value,
        "Emacs"
    );
    Ok(())
}

#[tokio::test]
async fn duplicate_observation_is_refused_without_a_new_revision() -> Result<()> {
    let tmp = TempDir::new()?;
    let root = tmp.path().to_path_buf();
    let service = profile_service(&root);
    service.observe_batch(&[Observation {
        section: SectionKind::Preferences,
        key: "editor".into(),
        value: "Neovim".into(),
        kind: ObservationKind::StableFact,
        confidence: 90,
    }])?;
    let revisions_before = revision_count(&root);
    let fingerprint_before = service.current()?.fingerprint;

    let trace: LlmTrace = serde_json::from_value(update_call(serde_json::json!([
        { "section": "preferences", "key": "editor", "value": "Neovim" }
    ])))?;
    let record = replay(&trace, Arc::clone(&service)).await?;
    assert!(
        tool_output_contains(&record, "duplicate of the current fact"),
        "history: {:?}",
        record.history
    );
    assert_eq!(
        revision_count(&root),
        revisions_before,
        "a duplicate must not add a revision"
    );
    assert_eq!(
        service.current()?.fingerprint,
        fingerprint_before,
        "the document rendering must not change"
    );
    Ok(())
}

#[tokio::test]
async fn unknown_section_arguments_are_tool_errors_not_panics() -> Result<()> {
    let tmp = TempDir::new()?;
    let service = profile_service(tmp.path());
    let trace: LlmTrace = serde_json::from_value(update_call(serde_json::json!([
        { "section": "nonsense", "key": "k", "value": "v" }
    ])))?;
    let record = replay(&trace, Arc::clone(&service)).await?;
    assert!(
        tool_output_contains(&record, "unknown profile section"),
        "history: {:?}",
        record.history
    );
    Ok(())
}

/// A revision line longer than the 8 KiB tail window: every log line is a
/// full snapshot, so a line may exceed the window, and the window may even
/// start inside a multi-byte character. The cheap tail probe must report an
/// indeterminate tail instead of failing, so reconciliation falls back to
/// the full rebuild and the commit path succeeds (ADR-SA-001).
#[test]
fn a_revision_line_longer_than_the_tail_window_reconciles_by_rebuild() -> Result<()> {
    let tmp = TempDir::new()?;
    let root = tmp.path().to_path_buf();
    let service = profile_service(&root);
    service.observe_batch(&[Observation {
        section: SectionKind::Preferences,
        key: "editor".into(),
        value: "Neovim".into(),
        kind: ObservationKind::StableFact,
        confidence: 90,
    }])?;

    // Simulate the cross-process writer: append a second revision whose
    // snapshot exceeds the tail window. The multi-byte run grows the user's
    // notes (the document body), and the ASCII padding appended after it
    // pins the window start inside the run deterministically.
    let log_path = root.join("user_profile.revisions.jsonl");
    let first = std::fs::read_to_string(&log_path)?;
    let line1 = first.lines().next().unwrap_or_default();
    let entry1: serde_json::Value = serde_json::from_str(line1)?;
    let snapshot_prefix = entry1["bytes"].as_str().unwrap_or_default().to_owned();
    let run = "汉".repeat(3_000);
    let line2 = {
        let mut entry = entry1.clone();
        entry["id"] = serde_json::json!(2);
        entry["note"] = serde_json::json!("externally written revision with a long snapshot");
        let mut padding = String::new();
        loop {
            entry["bytes"] = serde_json::json!(format!("{snapshot_prefix}{run}{padding}"));
            let line = entry.to_string();
            let run_start = line
                .as_bytes()
                .iter()
                .position(|byte| *byte >= 0x80)
                .unwrap_or(line.len());
            let suffix_len = line.len() - run_start - run.len();
            if line.len() >= 8_191 && suffix_len % 3 != 1 {
                break line;
            }
            padding.push('x');
        }
    };
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&log_path)?
        .write_all(format!("{line2}\n").as_bytes())?;

    // The next commit must reconcile through the full rebuild, not fail on
    // the truncated tail window.
    service.observe_batch(&[Observation {
        section: SectionKind::Preferences,
        key: "theme".into(),
        value: "dark".into(),
        kind: ObservationKind::StableFact,
        confidence: 90,
    }])?;
    assert_eq!(
        revision_count(&root),
        3,
        "the commit must reconcile and append after the oversized revision"
    );
    let state = service.current()?.state.expect("profile exists");
    assert_eq!(
        state
            .fact(SectionKind::Preferences, "editor")
            .expect("the pre-existing fact survives")
            .value,
        "Neovim"
    );
    assert_eq!(
        state
            .fact(SectionKind::Preferences, "theme")
            .expect("the new fact is committed")
            .value,
        "dark"
    );
    Ok(())
}
