//! M1 behavior-boundary tests. These exercise the real persistence and
//! use-case boundaries through the public API: atomic writes, external-edit
//! handling per document policy, Given negotiation (including the reject
//! path), rollback, the per-turn access point, and isolation from an
//! installed ZeroClaw.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use tempfile::TempDir;
use zeroclaw_super_agent::application::SuperAgentApp;
use zeroclaw_super_agent::application::ports::Clock;
use zeroclaw_super_agent::application::profile_service::ProfileService;
use zeroclaw_super_agent::application::self_service::{InitializationInput, SelfService};
use zeroclaw_super_agent::domain::amendment::{
    AmendmentRequest, ExperienceAuthority, ExperienceRef,
};
use zeroclaw_super_agent::domain::profile::{
    IgnoreReason, Observation, ObservationKind, RecordDecision, SectionKind,
};
use zeroclaw_super_agent::domain::selfhood::EarnedPatch;
use zeroclaw_super_agent::domain::selfhood::Gender;
use zeroclaw_super_agent::domain::selfhood::negotiation::{
    GivenAmendmentEvent, GivenAmendmentRequest, GivenField, GivenValue, RequestId, ValidationPolicy,
};
use zeroclaw_super_agent::domain::{DateYmd, RevisionId};
use zeroclaw_super_agent::infra;

/// Fixed clock so dates and timestamps are deterministic in tests.
struct FixedClock {
    today: DateYmd,
    now: u64,
}

impl Clock for FixedClock {
    fn today(&self) -> DateYmd {
        self.today
    }

    fn now_ms(&self) -> u64 {
        self.now
    }
}

fn clock() -> Arc<dyn Clock> {
    Arc::new(FixedClock {
        today: DateYmd::new(2026, 11, 3).expect("valid"),
        now: 1_000,
    })
}

fn self_service(root: &Path) -> SelfService {
    SelfService::new(
        infra::markdown_self_store(root).expect("store"),
        infra::jsonl_negotiation_log(root).expect("log"),
        clock(),
        Arc::new(ValidationPolicy),
    )
}

fn profile_service(root: &Path) -> ProfileService {
    ProfileService::new(
        zeroclaw_super_agent::infra::markdown_profile_store(root).expect("store"),
        clock(),
    )
}

fn app(root: &Path) -> SuperAgentApp {
    SuperAgentApp::new(self_service(root), profile_service(root))
}

fn init_input() -> InitializationInput {
    InitializationInput {
        name: "Aria".into(),
        birthday: DateYmd::new(2026, 1, 15).expect("valid"),
        gender: Gender::Undisclosed,
        initial_style: "warm and terse".into(),
    }
}

fn revision_kinds(root: &Path, log_name: &str) -> Vec<String> {
    let raw = std::fs::read_to_string(root.join(log_name)).expect("revision log exists");
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).expect("valid entry");
            value["kind"].as_str().expect("kind is a string").into()
        })
        .collect()
}

fn read_doc(root: &Path, name: &str) -> String {
    std::fs::read_to_string(root.join(name)).expect("document exists")
}

fn no_tmp_files(root: &Path) -> bool {
    std::fs::read_dir(root)
        .expect("root readable")
        .filter_map(|entry| entry.ok())
        .all(|entry| !entry.file_name().to_string_lossy().ends_with(".tmp"))
}

#[test]
fn initialize_is_validated_idempotent_and_persisted() -> Result<()> {
    let tmp = TempDir::new()?;
    let root = tmp.path();
    let service = self_service(root);

    service.initialize(init_input())?;
    // Idempotent: a second initialization is refused.
    assert!(service.initialize(init_input()).is_err());

    let loaded = service.current()?;
    let state = loaded.state.as_ref().expect("initialized");
    assert_eq!(state.given.name, "Aria");

    // A fresh store instance over the same directory sees the same state.
    let reloaded = self_service(root).current()?;
    assert_eq!(reloaded.state, loaded.state);
    assert!(no_tmp_files(root));
    Ok(())
}

#[test]
fn initialize_rejects_future_birthday_and_empty_name() -> Result<()> {
    let tmp = TempDir::new()?;
    let service = self_service(tmp.path());

    let future = InitializationInput {
        birthday: DateYmd::new(2027, 1, 1).expect("valid"),
        ..init_input()
    };
    let error = service.initialize(future).expect_err("rejected");
    assert!(error.to_string().contains("future"));

    let empty = InitializationInput {
        name: "   ".into(),
        ..init_input()
    };
    assert!(service.initialize(empty).is_err());

    // Nothing was written for rejected inputs.
    let loaded = service.current()?;
    assert!(loaded.state.is_none());
    Ok(())
}

#[test]
fn self_external_edit_is_restored_and_given_survives() -> Result<()> {
    let tmp = TempDir::new()?;
    let root = tmp.path();
    let service = self_service(root);
    service.initialize(init_input())?;
    let before = read_doc(root, "self.md");

    // The user hand-edits the Self document (not allowed by decision).
    let tampered = before.replace("Aria", "Someone Else");
    std::fs::write(root.join("self.md"), tampered)?;

    let loaded = service.current()?;
    assert!(matches!(
        loaded.external_edit,
        Some(zeroclaw_super_agent::application::ports::ExternalEditHandling::RestoredFromRevision)
    ));
    let state = loaded.state.as_ref().expect("restored");
    assert_eq!(state.given.name, "Aria", "given facts survive the restore");
    assert_eq!(
        read_doc(root, "self.md"),
        before,
        "file restored byte-exact"
    );

    // The next commit works from the restored fingerprint.
    let request = AmendmentRequest {
        patches: vec![EarnedPatch::SetTrait {
            kind: "patience".into(),
            strength: 40,
        }],
        evidence: vec![ExperienceRef {
            note: "observed across several sessions".into(),
        }],
    };
    let outcome = service.submit_amendment(request, &ExperienceAuthority)?;
    assert_eq!(outcome.revision.0, 2);
    assert!(no_tmp_files(root));
    Ok(())
}

#[test]
fn self_amendment_requires_evidence_and_touches_only_earned() -> Result<()> {
    let tmp = TempDir::new()?;
    let root = tmp.path();
    let service = self_service(root);
    service.initialize(init_input())?;

    let evidence_free = AmendmentRequest {
        patches: vec![EarnedPatch::SetTrait {
            kind: "patience".into(),
            strength: 40,
        }],
        evidence: vec![],
    };
    let error = service
        .submit_amendment(evidence_free, &ExperienceAuthority)
        .expect_err("denied");
    assert!(error.to_string().contains("denied"));

    let with_evidence = AmendmentRequest {
        patches: vec![EarnedPatch::SetTrait {
            kind: "patience".into(),
            strength: 40,
        }],
        evidence: vec![ExperienceRef {
            note: "user reinforced patient behavior".into(),
        }],
    };
    service.submit_amendment(with_evidence, &ExperienceAuthority)?;
    let state = service.current()?.state.expect("initialized");
    assert_eq!(state.given.name, "Aria", "given untouched by amendment");
    assert!(
        state
            .earned
            .traits
            .iter()
            .any(|t| t.kind == "patience" && t.strength == 40)
    );
    Ok(())
}

#[test]
fn negotiation_reject_path_is_recorded_and_changes_nothing() -> Result<()> {
    let tmp = TempDir::new()?;
    let root = tmp.path();
    let service = self_service(root);
    service.initialize(init_input())?;

    // Identical value: the individual rejects with a reason.
    let outcome = service.propose_given_change(
        GivenField::Name,
        GivenValue::Name("Aria".into()),
        "no reason needed".to_owned(),
    )?;
    assert!(!outcome.accepted);
    assert!(outcome.reason.contains("already the current value"));
    assert!(outcome.revision.is_none(), "I-N3: rejection writes nothing");

    // The negotiation log holds proposal + decision; the document log holds
    // only the initialization revision.
    let raw = std::fs::read_to_string(root.join("given_negotiation.jsonl"))?;
    assert_eq!(raw.lines().count(), 2);
    assert_eq!(
        revision_kinds(root, "self.revisions.jsonl"),
        vec!["initialize"]
    );
    assert!(service.pending_given_request()?.is_none());

    // A future birthday is likewise rejected with a reason.
    let outcome = service.propose_given_change(
        GivenField::Birthday,
        GivenValue::Birthday(DateYmd::new(2027, 1, 1).expect("valid")),
        "makes me older".to_owned(),
    )?;
    assert!(!outcome.accepted);
    assert!(outcome.reason.contains("future"));
    Ok(())
}

#[test]
fn negotiation_accept_changes_given_only_through_its_own_revision() -> Result<()> {
    let tmp = TempDir::new()?;
    let root = tmp.path();
    let service = self_service(root);
    service.initialize(init_input())?;

    let outcome = service.propose_given_change(
        GivenField::Name,
        GivenValue::Name("Vega".into()),
        "the user prefers it".to_owned(),
    )?;
    assert!(outcome.accepted);
    assert_eq!(
        outcome.reason,
        "the proposal passes the same field validation as initialization"
    );
    let revision = outcome.revision.expect("accepted writes a revision");
    assert_eq!(revision.0, 2);
    assert_eq!(
        revision_kinds(root, "self.revisions.jsonl"),
        vec!["initialize", "negotiated"]
    );
    let state = service.current()?.state.expect("present");
    assert_eq!(state.given.name, "Vega");
    Ok(())
}

#[test]
fn pending_request_blocks_proposals_until_withdrawn() -> Result<()> {
    let tmp = TempDir::new()?;
    let root = tmp.path();
    let service = self_service(root);
    service.initialize(init_input())?;

    // Simulate a crash between the Proposed event and its decision: append
    // a proposal with no matching Decided event.
    let request = GivenAmendmentRequest::proposed(
        RequestId(1),
        GivenField::Name,
        GivenValue::Name("Vega".into()),
        "crash mid-negotiation".into(),
        1,
    );
    std::fs::write(
        root.join("given_negotiation.jsonl"),
        serde_json::to_string(&GivenAmendmentEvent::Proposed { request })? + "\n",
    )?;
    assert!(service.pending_given_request()?.is_some());

    let error = service
        .propose_given_change(GivenField::Name, GivenValue::Name("Other".into()), "r")
        .expect_err("blocked by the open request");
    assert!(error.to_string().contains("already pending"));

    assert!(service.withdraw_given_request(RequestId(1))?);
    assert!(
        !service.withdraw_given_request(RequestId(1))?,
        "already withdrawn"
    );
    assert!(service.pending_given_request()?.is_none());

    // After withdrawal the same field can be proposed again.
    let outcome =
        service.propose_given_change(GivenField::Name, GivenValue::Name("Vega".into()), "retry")?;
    assert!(outcome.accepted);
    Ok(())
}

#[test]
fn rollback_restores_the_previous_revision() -> Result<()> {
    let tmp = TempDir::new()?;
    let root = tmp.path();
    let service = profile_service(root);

    service.observe_batch(&[fact_observation("editor", "Neovim")])?;
    service.observe_batch(&[fact_observation("editor", "Helix")])?;
    assert_eq!(
        revision_kinds(root, "user_profile.revisions.jsonl").len(),
        2
    );
    assert_eq!(
        service
            .current()?
            .state
            .expect("present")
            .fact(SectionKind::Preferences, "editor")
            .expect("present")
            .value,
        "Helix"
    );

    service.rollback(RevisionId(1))?;
    assert_eq!(
        service
            .current()?
            .state
            .expect("present")
            .fact(SectionKind::Preferences, "editor")
            .expect("present")
            .value,
        "Neovim"
    );
    let kinds = revision_kinds(root, "user_profile.revisions.jsonl");
    assert_eq!(kinds.last().map(String::as_str), Some("rollback"));
    Ok(())
}

#[test]
fn profile_external_edit_is_absorbed_as_a_user_revision() -> Result<()> {
    let tmp = TempDir::new()?;
    let root = tmp.path();
    let service = profile_service(root);
    service.observe_batch(&[fact_observation("editor", "Neovim")])?;

    // The user hand-edits the profile body (allowed by decision). The body
    // is the notes area; rewrite it while keeping the front matter intact.
    let before = read_doc(root, "user_profile.md");
    let cut = before.rfind("\n+++\n").expect("closing delimiter");
    let edited = format!("{}+++\nuser rewrote this\n", &before[..cut + 1]);
    std::fs::write(root.join("user_profile.md"), edited)?;

    let loaded = service.current()?;
    assert!(matches!(
        loaded.external_edit,
        Some(zeroclaw_super_agent::application::ports::ExternalEditHandling::AbsorbedAsUserEdit)
    ));
    assert_eq!(
        loaded.state.expect("present").notes,
        "user rewrote this\n",
        "notes are preserved byte-exact"
    );
    assert!(
        revision_kinds(root, "user_profile.revisions.jsonl")
            .contains(&"user_explicit_edit".to_string())
    );
    Ok(())
}

#[test]
fn cross_subject_writes_stay_isolated() -> Result<()> {
    let tmp = TempDir::new()?;
    let root = tmp.path();
    let agent = app(root);
    agent.self_service().initialize(init_input()).expect("init");
    let self_before = read_doc(root, "self.md");

    // Profile writes never touch the Self document (I-X2).
    let outcomes = agent
        .profile_service()
        .observe_batch(&[fact_observation("editor", "Neovim")])?;
    assert!(matches!(outcomes[0].decision, RecordDecision::Record));
    assert_eq!(read_doc(root, "self.md"), self_before);

    // Self amendments never touch the profile document.
    let profile_before = read_doc(root, "user_profile.md");
    agent
        .self_service()
        .submit_amendment(
            AmendmentRequest {
                patches: vec![EarnedPatch::SetTrait {
                    kind: "patience".into(),
                    strength: 30,
                }],
                evidence: vec![ExperienceRef {
                    note: "observed".into(),
                }],
            },
            &ExperienceAuthority,
        )
        .expect("amended");
    assert_eq!(read_doc(root, "user_profile.md"), profile_before);
    Ok(())
}

#[test]
fn profile_budget_splits_record_from_needs_review() -> Result<()> {
    let tmp = TempDir::new()?;
    let root = tmp.path();
    let service = profile_service(root);

    let observations: Vec<Observation> = ["a", "b", "c", "d", "e"]
        .iter()
        .map(|key| fact_observation(key, "value"))
        .collect();
    let outcomes = service.observe_batch(&observations)?;
    let recorded = outcomes
        .iter()
        .filter(|o| matches!(o.decision, RecordDecision::Record))
        .count();
    let needs_review = outcomes
        .iter()
        .filter(|o| matches!(o.decision, RecordDecision::NeedsReview))
        .count();
    assert_eq!(recorded, 3, "per-turn budget");
    assert_eq!(needs_review, 2);
    let state = service.current()?.state.expect("present");
    assert_eq!(state.fact_count(), 3);
    Ok(())
}

#[test]
fn credentials_never_reach_the_profile() -> Result<()> {
    let tmp = TempDir::new()?;
    let service = profile_service(tmp.path());
    let outcomes = service.observe_batch(&[Observation {
        section: SectionKind::Environment,
        key: "api_key".into(),
        value: "sk-abcdef1234567890".into(),
        kind: ObservationKind::Correction,
        confidence: 100,
    }])?;
    assert!(matches!(
        outcomes[0].decision,
        RecordDecision::Ignore {
            reason: IgnoreReason::CredentialLike
        }
    ));
    // Nothing recordable means nothing was written, not even an empty file.
    let loaded = service.current()?;
    assert!(loaded.state.is_none());
    Ok(())
}

#[test]
fn begin_turn_reads_both_documents_fresh() -> Result<()> {
    let tmp = TempDir::new()?;
    let root = tmp.path();
    let agent = app(root);
    agent.self_service().initialize(init_input()).expect("init");

    let first = agent.begin_turn()?;
    assert!(first.capabilities.is_none(), "no generation installed yet");
    assert!(first.self_narrative.contains("Aria"));
    assert!(first.user_profile_context.is_empty(), "no facts yet");

    // A new fact is visible on the very next turn — the access point never
    // caches document state.
    agent
        .profile_service()
        .observe_batch(&[fact_observation("editor", "Neovim")])?;
    let second = agent.begin_turn()?;
    assert!(second.user_profile_context.contains("Neovim"));
    Ok(())
}

#[test]
fn default_data_root_is_isolated_from_installed_zeroclaw() {
    use zeroclaw_super_agent::infra::paths;

    assert!(
        paths::isolation_holds(),
        "default data root must not resolve to the installed ZeroClaw's ~/.zeroclaw"
    );
    let root = paths::default_data_root().expect("home available");
    assert_eq!(
        root.file_name().and_then(|name| name.to_str()),
        Some(".kangaroo")
    );
    assert_eq!(paths::DEFAULT_GATEWAY_PORT, 42_618);
    assert_eq!(paths::BINARY_NAME, "kangaroo");
}

fn fact_observation(key: &str, value: &str) -> Observation {
    Observation {
        section: SectionKind::Preferences,
        key: key.into(),
        value: value.into(),
        kind: ObservationKind::StableFact,
        confidence: 90,
    }
}
