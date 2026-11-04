//! The `kangaroo consolidate` adapter (memory design, Phase B): process the
//! transcript journal after the cursor, one LLM-bounded window per call,
//! and write winners through the episode store. The cursor advances only
//! after a window's records are durably written; a failure anywhere leaves
//! it in place, so the next run redoes exactly that window (idempotent via
//! anchor dedup). Nightly scheduling is external (e.g. Task Scheduler).

use anyhow::{Context, Result};
use std::path::Path;

use zeroclaw_api::model_provider::{ChatMessage, ChatRequest};
use zeroclaw_super_agent::infra::consolidation::{
    build_prompt, candidate_lines, parse_proposals, select_winners, window_slice,
};
use zeroclaw_super_agent::infra::episodes::{
    ConsolidationCursor, EpisodeStore, cursor_path, episodes_path, load_cursor, save_cursor,
};
use zeroclaw_super_agent::infra::transcript::{parse_entry, transcripts_dir};

/// LLM call budget for one consolidation invocation (the user's decision:
/// over budget means stop, cursor stays, next run catches up).
const MAX_LLM_CALLS_PER_INVOCATION: u32 = 4;

fn fluent(key: &str) -> String {
    zeroclaw_runtime::i18n::get_required_cli_string(key)
}

fn fluent_args(key: &str, args: &[(&str, &str)]) -> String {
    zeroclaw_runtime::i18n::get_required_cli_string_with_args(key, args)
}

/// Pick the oldest day file with unprocessed raw lines after the cursor.
fn pick_window(
    day_files: &[String],
    transcript_dir: &Path,
    cursor: &ConsolidationCursor,
) -> Result<Option<(String, Vec<String>, u64)>> {
    let start_index = if cursor.file.is_empty() {
        0
    } else {
        day_files
            .iter()
            .position(|name| name == &cursor.file)
            .with_context(|| format!("cursor file {} not in transcripts", cursor.file))?
    };
    for (index, name) in day_files.iter().enumerate().skip(start_index) {
        let raw = std::fs::read_to_string(transcript_dir.join(name))
            .with_context(|| format!("transcript read failed: {name}"))?;
        let total = raw.lines().count() as u64;
        let offset = if index == start_index && name == &cursor.file {
            cursor.line_offset.min(total)
        } else {
            0
        };
        if offset < total {
            return Ok(Some((
                name.clone(),
                raw.lines().map(str::to_owned).collect(),
                offset,
            )));
        }
    }
    Ok(None)
}

/// One consolidation invocation: up to [`MAX_LLM_CALLS_PER_INVOCATION`]
/// windows; audited skips (all anchors already covered) do not consume LLM
/// calls.
pub async fn run(root: &Path, config: &zeroclaw_config::schema::Config) -> Result<()> {
    let provider_ref = config
        .agents
        .get("kangaroo")
        .map(|agent| agent.model_provider.to_string())
        .filter(|reference| !reference.is_empty())
        .context("consolidation needs [agents.kangaroo].model_provider")?;
    let transcript_dir = transcripts_dir(root);
    let store = EpisodeStore::new(episodes_path(root));
    let cursor_file = cursor_path(root);
    let mut cursor = load_cursor(&cursor_file)?.unwrap_or_default();

    let mut day_files: Vec<String> = std::fs::read_dir(&transcript_dir)
        .with_context(|| format!("transcripts dir read failed: {}", transcript_dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.ends_with(".jsonl"))
        .collect();
    day_files.sort();

    let mut covered = store.covered_msg_ids()?;
    let mut llm_calls = 0u32;
    let mut windows = 0usize;
    let mut episodes = 0usize;
    let mut hypotheses = 0usize;
    let mut audited = 0usize;

    while llm_calls < MAX_LLM_CALLS_PER_INVOCATION {
        let Some((file_name, raw_lines, offset)) =
            pick_window(&day_files, &transcript_dir, &cursor)?
        else {
            break;
        };
        let parsed: Vec<(
            u64,
            zeroclaw_super_agent::infra::transcript::TranscriptEntry,
        )> = raw_lines
            .iter()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .filter_map(|(index, line)| {
                // Trace lines are 1-based over the raw file, matching the
                // episodes layer contract (see episodes::EpisodeTrace).
                parse_entry(line)
                    .ok()
                    .map(|entry| (index as u64 + 1, entry))
            })
            .collect();
        let remaining: Vec<(u64, _)> = parsed
            .iter()
            .filter(|(index, _)| *index >= offset)
            .cloned()
            .collect();
        if remaining.is_empty() {
            // This file is fully processed; move the cursor to the next one.
            let next = day_files
                .iter()
                .position(|name| name == &file_name)
                .and_then(|position| day_files.get(position + 1))
                .cloned();
            cursor = match next {
                Some(next_file) => ConsolidationCursor {
                    file: next_file,
                    line_offset: 0,
                },
                None => ConsolidationCursor {
                    line_offset: raw_lines.len() as u64,
                    file: file_name.clone(),
                },
            };
            save_cursor(&cursor_file, &cursor)?;
            continue;
        }
        let (window, _end_index) = window_slice(&remaining, 0);
        let candidates = candidate_lines(&file_name, &window);
        let uncovered: Vec<&zeroclaw_super_agent::infra::consolidation::CandidateLine> = candidates
            .iter()
            .filter(|c| !covered.contains(&c.msg_id))
            .collect();
        if uncovered.is_empty() {
            // Audited skip: every anchor in this window already has an
            // episode; advance the cursor without spending an LLM call.
            audited += candidates.len();
            advance_cursor(
                &mut cursor,
                &cursor_file,
                &day_files,
                &file_name,
                window.last().map(|(index, _)| *index as usize),
                raw_lines.len(),
            )?;
            continue;
        }
        let anchors: std::collections::HashMap<String, (u64, String)> = candidates
            .iter()
            .map(|c| (c.msg_id.clone(), (c.line, c.ts.clone())))
            .collect();
        let existing = store.load_all()?;
        let prompt = build_prompt(&candidates, &existing);

        let resolved =
            zeroclaw_providers::create_model_provider_from_ref_with_model(config, &provider_ref)?;
        let model = resolved
            .model
            .clone()
            .context("provider alias carries no model for consolidation")?;
        let reply = resolved
            .provider
            .chat(
                ChatRequest {
                    messages: &[ChatMessage::user(prompt)],
                    tools: None,
                    thinking: None,
                },
                &model,
                Some(0.2),
            )
            .await
            .context("consolidation LLM call failed")?;
        llm_calls += 1;

        let proposals = parse_proposals(reply.text_or_empty())?;
        let outcome = select_winners(&proposals, &file_name, &anchors, &covered);
        for winner in &outcome.winners {
            covered.insert(winner.trace.msg_id.clone());
        }
        episodes += outcome
            .winners
            .iter()
            .filter(|w| w.kind == "episode")
            .count();
        hypotheses += outcome
            .winners
            .iter()
            .filter(|w| w.kind == "hypothesis")
            .count();
        store.append(&outcome.winners)?;
        advance_cursor(
            &mut cursor,
            &cursor_file,
            &day_files,
            &file_name,
            window.last().map(|(index, _)| *index as usize),
            raw_lines.len(),
        )?;
        windows += 1;
    }

    println!(
        "{}",
        fluent_args(
            "cli-consolidate-summary",
            &[
                ("windows", &windows.to_string()),
                ("llm_calls", &llm_calls.to_string()),
                ("episodes", &episodes.to_string()),
                ("hypotheses", &hypotheses.to_string()),
                ("audited", &audited.to_string()),
                ("cursor_file", &cursor.file),
                ("cursor_offset", &cursor.line_offset.to_string()),
            ]
        )
    );
    if llm_calls >= MAX_LLM_CALLS_PER_INVOCATION {
        println!("{}", fluent("cli-consolidate-budget-limited"));
    }
    Ok(())
}

/// Advance the cursor to just past the last processed raw line, or roll it
/// to the next day file when the current one is exhausted.
fn advance_cursor(
    cursor: &mut ConsolidationCursor,
    cursor_file: &Path,
    day_files: &[String],
    current_file: &str,
    last_processed_raw_index: Option<usize>,
    total_raw_lines: usize,
) -> Result<()> {
    let exhausted = last_processed_raw_index
        .map(|index| index + 1 >= total_raw_lines)
        .unwrap_or(true);
    *cursor = if exhausted {
        let next = day_files
            .iter()
            .position(|name| name == current_file)
            .and_then(|position| day_files.get(position + 1))
            .cloned();
        match next {
            Some(next_file) => ConsolidationCursor {
                file: next_file,
                line_offset: 0,
            },
            None => ConsolidationCursor {
                file: current_file.to_owned(),
                line_offset: total_raw_lines as u64,
            },
        }
    } else {
        ConsolidationCursor {
            file: current_file.to_owned(),
            line_offset: (last_processed_raw_index.expect("index present when not exhausted") + 1)
                as u64,
        }
    };
    save_cursor(cursor_file, cursor)
}
