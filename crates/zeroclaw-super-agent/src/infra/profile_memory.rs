//! The per-turn projection injection: wraps the real memory backend and
//! prepends the Self + UserProfile projections as a pinned recall entry, so
//! the runtime's own `[Memory context]` pipeline carries them every turn —
//! without touching `zeroclaw-runtime` and without the projections ever
//! being written into the memory store (they are read from the documents on
//! every recall).

use std::sync::Arc;

use async_trait::async_trait;
use zeroclaw_api::attribution::Attributable;
use zeroclaw_api::attribution::{MemoryKind, Role};
use zeroclaw_api::memory_traits::{Memory, MemoryCategory, MemoryEntry};

use crate::application::profile_service::ProfileService;
use crate::application::self_service::SelfService;

const PROJECTION_KEY: &str = "kangaroo-projection";

pub struct ProfileMemoryAdapter {
    inner: Arc<dyn Memory>,
    self_service: Arc<SelfService>,
    profile_service: Arc<ProfileService>,
}

impl ProfileMemoryAdapter {
    pub fn new(
        inner: Arc<dyn Memory>,
        self_service: Arc<SelfService>,
        profile_service: Arc<ProfileService>,
    ) -> Self {
        Self {
            inner,
            self_service,
            profile_service,
        }
    }

    /// Reads both documents fresh and renders the dual projection. Each turn
    /// re-reads from disk; nothing is cached here (design §3.6).
    async fn projection_entry(&self) -> anyhow::Result<Option<MemoryEntry>> {
        let self_projection = self.self_service.projection()?;
        if !self_projection.initialized {
            return Ok(None);
        }
        let profile_projection = self.profile_service.projection()?;
        let mut content = format!(
            "[Kangaroo identity]\n{}\n[About the user]\n{}",
            self_projection.narrative, profile_projection.context
        );
        content.truncate(3_000);
        Ok(Some(MemoryEntry {
            id: PROJECTION_KEY.to_owned(),
            key: PROJECTION_KEY.to_owned(),
            content,
            category: MemoryCategory::Core,
            timestamp: String::new(),
            session_id: None,
            score: Some(1.0),
            namespace: "default".into(),
            importance: Some(1.0),
            superseded_by: None,
            kind: None,
            pinned: true,
            tenant_id: None,
            principal_id: None,
            agent_alias: None,
            agent_id: None,
        }))
    }
}

impl Attributable for ProfileMemoryAdapter {
    fn role(&self) -> Role {
        Role::Memory(MemoryKind::InMemory)
    }

    fn alias(&self) -> &str {
        "kangaroo-profile-adapter"
    }
}

#[async_trait]
impl Memory for ProfileMemoryAdapter {
    fn name(&self) -> &str {
        "kangaroo-profile-adapter"
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.inner.store(key, content, category, session_id).await
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let mut entries = self
            .inner
            .recall(
                query,
                limit.saturating_sub(1).max(1),
                session_id,
                since,
                until,
            )
            .await?;
        if let Some(projection) = self.projection_entry().await? {
            entries.insert(0, projection);
        }
        entries.truncate(limit);
        Ok(entries)
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        self.inner.get(key).await
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.inner.list(category, session_id).await
    }

    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        self.inner.forget(key).await
    }

    async fn forget_for_agent(&self, key: &str, agent_id: &str) -> anyhow::Result<bool> {
        self.inner.forget_for_agent(key, agent_id).await
    }

    async fn count(&self) -> anyhow::Result<usize> {
        self.inner.count().await
    }

    async fn health_check(&self) -> bool {
        self.inner.health_check().await
    }

    async fn store_with_agent(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        namespace: Option<&str>,
        importance: Option<f64>,
        agent_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.inner
            .store_with_agent(
                key, content, category, session_id, namespace, importance, agent_id,
            )
            .await
    }

    async fn recall_for_agents(
        &self,
        allowed_agent_ids: &[&str],
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.inner
            .recall_for_agents(allowed_agent_ids, query, limit, session_id, since, until)
            .await
    }
}
