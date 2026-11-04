//! The questionnaire interaction adapter (outer layer): placeholder
//! questions, prompts, in-session progress, and answer→draft mapping. This
//! is replaceable data + interaction mechanics — the core never sees
//! questions, ids, progress, or the not-disclosed sentinel.
//!
//! In-session only: all state is process memory; an abandoned questionnaire
//! writes nothing and restarts next launch.

use anyhow::Result;
use std::io::Write;
use std::sync::Arc;

use zeroclaw_super_agent::application::initialization::InitializationDraft;
use zeroclaw_super_agent::application::profile_service::ProfileService;
use zeroclaw_super_agent::application::self_service::SelfService;
use zeroclaw_super_agent::domain::DateYmd;
use zeroclaw_super_agent::domain::profile::{Observation, ObservationKind, SectionKind};
use zeroclaw_super_agent::domain::selfhood::negotiation::GivenField;
use zeroclaw_super_agent::domain::selfhood::{Gender, GivenFacts};

/// Explicit marker recorded when the respondent declines an optional
/// question. Data-level text; the adapter owns it, not the core.
pub const NOT_DISCLOSED: &str = "(not disclosed)";

/// Assumption tier for hidden-need answers (explicit user statement, lower
/// provenance than a conversation correction).
pub const ASSUMPTION_CONFIDENCE: u8 = 75;

struct PlaceholderQuestion {
    prompt_key: &'static str,
    target: Target,
    required: bool,
}

enum Target {
    Given(GivenField),
    Assumption {
        section: SectionKind,
        key: &'static str,
    },
}

/// The placeholder question set (replaceable data, 待改版): the
/// expectation-guided redesign will replace this wholesale.
fn placeholder_questions() -> Vec<PlaceholderQuestion> {
    vec![
        PlaceholderQuestion {
            prompt_key: "cli-intake-q-name",
            target: Target::Given(GivenField::Name),
            required: true,
        },
        PlaceholderQuestion {
            prompt_key: "cli-intake-q-birthday",
            target: Target::Given(GivenField::Birthday),
            required: true,
        },
        PlaceholderQuestion {
            prompt_key: "cli-intake-q-gender",
            target: Target::Given(GivenField::Gender),
            required: false,
        },
        PlaceholderQuestion {
            prompt_key: "cli-intake-q-style",
            target: Target::Given(GivenField::InitialStyle),
            required: false,
        },
        PlaceholderQuestion {
            prompt_key: "cli-intake-q-assumption",
            target: Target::Assumption {
                section: SectionKind::Preferences,
                key: "communication-style",
            },
            required: false,
        },
    ]
}

/// Runs the in-session questionnaire and returns the assembled draft. All
/// interaction state is local; nothing touches the documents until the
/// caller commits the draft through the core use case.
pub fn run_questionnaire(
    stdin: &std::io::Stdin,
    out: &mut dyn Write,
) -> Result<InitializationDraft> {
    let questions = placeholder_questions();
    let total = questions.len();
    let mut name = NOT_DISCLOSED.to_owned();
    let mut birthday: Option<DateYmd> = None;
    let mut gender = Gender::Undisclosed;
    let mut initial_style = "neutral".to_owned();
    let mut assumptions: Vec<Observation> = Vec::new();

    for (index, question) in questions.iter().enumerate() {
        let text = zeroclaw_runtime::i18n::get_required_cli_string(question.prompt_key);
        write!(out, "[{}/{}] {text} ", index + 1, total)?;
        out.flush()?;
        let mut line = String::new();
        stdin.read_line(&mut line)?;
        let answer = line.trim().to_owned();
        if answer == ":quit" {
            anyhow::bail!(
                "{}",
                zeroclaw_runtime::i18n::get_required_cli_string("cli-intake-abandoned")
            );
        }
        let declined = answer.is_empty() && !question.required;
        if answer.is_empty() && question.required {
            anyhow::bail!(
                "{}",
                zeroclaw_runtime::i18n::get_required_cli_string("cli-intake-required")
            );
        }
        match (&question.target, declined) {
            (Target::Given(GivenField::Name), false) => name = answer,
            (Target::Given(GivenField::Name), true) => name = NOT_DISCLOSED.to_owned(),
            (Target::Given(GivenField::Birthday), false) => {
                birthday = Some(
                    DateYmd::parse(&answer)
                        .map_err(|reason| anyhow::Error::msg(format!("birthday: {reason}")))?,
                );
            }
            (Target::Given(GivenField::Gender), false) => {
                gender = Gender::parse(&answer)
                    .ok_or_else(|| anyhow::Error::msg(format!("unknown gender: {answer}")))?;
            }
            (Target::Given(GivenField::Gender), true) => gender = Gender::Undisclosed,
            (Target::Given(GivenField::InitialStyle), false) => initial_style = answer,
            (Target::Given(GivenField::InitialStyle), true) => initial_style = "neutral".into(),
            (Target::Assumption { section, key }, false) => assumptions.push(Observation {
                section: *section,
                key: (*key).to_owned(),
                value: answer,
                kind: ObservationKind::StableFact,
                confidence: ASSUMPTION_CONFIDENCE,
            }),
            (Target::Assumption { .. }, true) => {}
            (Target::Given(GivenField::Birthday), true) => {
                anyhow::bail!(
                    "{}",
                    zeroclaw_runtime::i18n::get_required_cli_string("cli-intake-required")
                );
            }
        }
    }
    let given = GivenFacts {
        name,
        birthday: birthday.ok_or_else(|| {
            anyhow::Error::msg("the birthday question is required and was not answered")
        })?,
        gender,
        initial_style,
    };
    Ok(InitializationDraft { given, assumptions })
}

/// Commits a completed draft: core initialization first (atomic, audited),
/// then the draft's assumptions through the profile's own recording plan.
pub fn commit_draft(
    draft: InitializationDraft,
    self_service: &Arc<SelfService>,
    profile_service: &Arc<ProfileService>,
) -> Result<u64> {
    let revision = self_service.initialize_from_draft(draft.clone())?;
    let _ = profile_service.observe_batch(&draft.assumptions);
    Ok(revision.0)
}
