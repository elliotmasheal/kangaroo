//! Core-boundary gate (wiring milestone architecture correction): the
//! questionnaire is an OUTER interaction adapter (`src/bin/kangaroo/`), and
//! the core crate must not carry it. The lib surface must not reference
//! question-set concepts, and the core manifest must not grow a dependency
//! on any adapter crate. Dependency direction enforced by test, not by
//! convention.

const CORE_SOURCES: &[(&str, &str)] = &[
    ("src/lib.rs", include_str!("../src/lib.rs")),
    (
        "src/application/mod.rs",
        include_str!("../src/application/mod.rs"),
    ),
    (
        "src/application/initialization.rs",
        include_str!("../src/application/initialization.rs"),
    ),
    (
        "src/application/self_service.rs",
        include_str!("../src/application/self_service.rs"),
    ),
    (
        "src/application/profile_service.rs",
        include_str!("../src/application/profile_service.rs"),
    ),
    (
        "src/application/super_agent_app.rs",
        include_str!("../src/application/super_agent_app.rs"),
    ),
    (
        "src/application/ports.rs",
        include_str!("../src/application/ports.rs"),
    ),
];

/// Questionnaire/interaction-adapter concepts that must never appear in the
/// core's library surface.
const FORBIDDEN_CONCEPTS: &[&str] = &[
    "IntakeQuestion",
    "IntakeScript",
    "IntakeDraftStore",
    "questionnaire",
    "cli-intake-q-",
    "NOT_DISCLOSED",
    "next_question",
];

const CORE_MANIFEST: &str = include_str!("../Cargo.toml");

#[test]
fn core_library_surface_carries_no_questionnaire_concepts() {
    for (file, source) in CORE_SOURCES {
        for token in FORBIDDEN_CONCEPTS {
            assert!(
                !source.contains(token),
                "{file} mentions {token:?}: questionnaire concepts belong to the \
                 outer interaction adapter (src/bin/kangaroo/), not the core"
            );
        }
    }
}

#[test]
fn core_manifest_has_no_interaction_adapter_dependency() {
    assert!(
        !CORE_MANIFEST.contains("kangaroo-cli"),
        "the core crate must not depend on an interaction adapter crate"
    );
    assert!(
        !CORE_MANIFEST.contains("path = \"../../apps/"),
        "the core crate must not reach into app crates"
    );
}
