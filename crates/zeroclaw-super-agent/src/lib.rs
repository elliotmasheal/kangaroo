//! Independent-agent domain layer for ZeroClaw.
//!
//! Hosts two cognition documents as separate single sources of truth: the
//! Self aggregate (given facts + earned traits, experience-driven) and the
//! UserProfile aggregate (facts about the user, user-editable). Given facts
//! change only through negotiation; Earned traits change only through
//! experience-driven `SelfAmendment` behind an `AmendmentAuthority`.
//!
//! Layering: `domain` is pure (no IO), `application` orchestrates through
//! ports, `infra` persists Markdown documents with TOML front matter plus
//! JSONL revision logs. Capability wiring for the runtime agent loop arrives
//! through `zeroclaw_runtime::composition::RuntimeCapabilities` (see
//! `application::super_agent_app`).

pub mod application;
pub mod domain;
pub mod infra;
pub mod tools;
