//! The turn entry point: per-turn projections of both cognition documents
//! plus the capability-generation snapshot the caller passes into the
//! runtime agent loop (`agent::run_with_capabilities` in M2 wiring).

use anyhow::Result;
use std::sync::{Arc, RwLock, RwLockReadGuard};
use zeroclaw_runtime::composition::RuntimeCapabilities;

use crate::application::profile_service::ProfileService;
use crate::application::self_service::SelfService;

/// What one turn starts with. `capabilities` is the generation snapshot:
/// a turn that starts under one generation finishes with the capabilities it
/// started with (composition contract); the caller forwards it to the
/// runtime entry point unchanged.
#[derive(Clone)]
pub struct TurnStart {
    pub capabilities: Option<Arc<RuntimeCapabilities>>,
    /// Self projection (identity, earned traits, time anchors).
    pub self_narrative: String,
    /// UserProfile projection (facts + the user's own notes).
    pub user_profile_context: String,
}

pub struct SuperAgentApp {
    /// Current config generation handle. Only the handle is held; provider,
    /// memory and tool instances resolve through the capability sources on
    /// demand — never cached across generations.
    generation: RwLock<Option<Arc<RuntimeCapabilities>>>,
    self_service: SelfService,
    profile_service: ProfileService,
}

impl SuperAgentApp {
    pub fn new(self_service: SelfService, profile_service: ProfileService) -> Self {
        Self {
            generation: RwLock::new(None),
            self_service,
            profile_service,
        }
    }

    /// Called by the wiring layer when a live config apply builds a new
    /// generation. Turns already in flight keep their snapshot; new turns
    /// pick this one up.
    pub fn install_generation(&self, capabilities: Arc<RuntimeCapabilities>) {
        // A poisoned lock still holds valid data; recover into the guard
        // instead of panicking (production paths never unwrap/expect).
        let mut guard = self
            .generation
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(capabilities);
    }

    pub fn self_service(&self) -> &SelfService {
        &self.self_service
    }

    pub fn profile_service(&self) -> &ProfileService {
        &self.profile_service
    }

    /// The per-turn access point: reads both documents fresh (no caching)
    /// and snapshots the current capability generation.
    pub fn begin_turn(&self) -> Result<TurnStart> {
        let capabilities = self.current_generation();
        let self_narrative = self.self_service.projection()?.narrative;
        let user_profile_context = self.profile_service.projection()?.context;
        Ok(TurnStart {
            capabilities,
            self_narrative,
            user_profile_context,
        })
    }

    fn current_generation(&self) -> Option<Arc<RuntimeCapabilities>> {
        let guard: RwLockReadGuard<'_, Option<Arc<RuntimeCapabilities>>> = self
            .generation
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.clone()
    }
}
