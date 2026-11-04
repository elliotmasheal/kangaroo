//! Host assembly for the WebUI turn: the product supplies its own memory
//! handle (the dual-projection adapter) as a `MemorySource`, so a supplied
//! capability set never falls back to a config-backed store behind it.

use std::sync::Arc;

use async_trait::async_trait;
use zeroclaw_api::memory_traits::Memory;
use zeroclaw_runtime::composition::{MemoryRequest, MemorySource, RuntimeCapabilities};

/// A `MemorySource` that always hands back the host's one handle. The
/// supplied-capabilities contract guarantees two requests in one generation
/// reach the same store; the host adapter already is that store.
struct FixedMemorySource {
    memory: Arc<dyn Memory>,
}

#[async_trait]
impl MemorySource for FixedMemorySource {
    async fn memory(&self, _request: &MemoryRequest<'_>) -> anyhow::Result<Arc<dyn Memory>> {
        Ok(Arc::clone(&self.memory))
    }
}

/// Return `capabilities` with the memory source replaced by `memory`. Every
/// other source (providers, tools, channels, observer) is kept as-is, so the
/// caller decides which seams the product replaces.
pub fn with_supplied_memory(
    capabilities: RuntimeCapabilities,
    memory: Arc<dyn Memory>,
) -> RuntimeCapabilities {
    RuntimeCapabilities {
        memory: Arc::new(FixedMemorySource { memory }),
        ..capabilities
    }
}
