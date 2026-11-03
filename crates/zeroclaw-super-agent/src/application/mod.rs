//! Application layer: use cases over the domain aggregates, wired through
//! ports ([`ports`]). The turn entry point and the capability-generation
//! handle live in [`super_agent_app`].

pub mod ports;
pub mod profile_service;
pub mod self_service;
pub mod super_agent_app;

pub use super_agent_app::{SuperAgentApp, TurnStart};
