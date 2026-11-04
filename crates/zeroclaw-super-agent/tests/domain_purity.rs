//! Compile-time domain purity gate (design doc DDD self-check #7): the
//! domain layer's source files must not reference infrastructure concerns.
//! The sources are embedded here, so a violation fails this crate's test
//! build in the same process as everything else.

const DOMAIN_SOURCES: &[(&str, &str)] = &[
    ("domain/mod.rs", include_str!("../src/domain/mod.rs")),
    (
        "domain/selfhood/mod.rs",
        include_str!("../src/domain/selfhood/mod.rs"),
    ),
    (
        "domain/selfhood/negotiation.rs",
        include_str!("../src/domain/selfhood/negotiation.rs"),
    ),
    (
        "domain/profile/mod.rs",
        include_str!("../src/domain/profile/mod.rs"),
    ),
    (
        "domain/amendment/mod.rs",
        include_str!("../src/domain/amendment/mod.rs"),
    ),
    (
        "domain/orchestration.rs",
        include_str!("../src/domain/orchestration.rs"),
    ),
];

/// Technical concerns that must never appear in domain source: persistence
/// (serde, TOML/JSON), IO (fs, io), async runtime, capability handles,
/// logging, the error-context crate (domain errors are plain strings), and
/// any other zeroclaw crate.
const FORBIDDEN: &[&str] = &[
    "serde",
    "tokio",
    "std::fs",
    "std::io",
    "cap_std",
    "anyhow",
    "zeroclaw_",
    "async ",
    "Mutex",
    "OnceLock",
    "include_str",
];

#[test]
fn domain_sources_carry_no_infrastructure_details() {
    for (file, source) in DOMAIN_SOURCES {
        for token in FORBIDDEN {
            assert!(
                !source.contains(token),
                "{file} mentions {token:?}: the domain layer must stay free of infrastructure details"
            );
        }
    }
}

#[test]
fn domain_sources_are_nonempty() {
    for (file, source) in DOMAIN_SOURCES {
        assert!(source.contains("pub "), "{file} looks empty");
    }
}
