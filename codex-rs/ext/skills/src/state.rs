use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;

use codex_core_skills::HostSkillsSnapshot;
use codex_extension_api::ConversationHistory;
use codex_mcp::McpResourceClient;
use codex_mcp::McpResourceClientCacheKey;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use tokio::sync::OnceCell;

use crate::SkillsExtensionConfig;
use crate::catalog::SkillAuthority;
use crate::catalog::SkillCatalog;
use crate::catalog::SkillCatalogEntry;
use crate::catalog::SkillPackageId;
use crate::catalog::SkillProviderError;
use crate::catalog::SkillProviderResult;
use crate::catalog::SkillReadResult;
use crate::catalog::SkillResourceId;
use crate::catalog::SkillSourceKind;
use crate::fragments::AvailableSkillsInstructions;
use crate::fragments::PromotedSkillIdentity;
use crate::fragments::promoted_metadata_is_bounded;
use crate::provider::SkillListQuery;
use crate::provider::SkillReadRequest;
use crate::sources::SkillProviders;

const MAX_CACHED_ORCHESTRATOR_RESOURCES: usize = 100;
const MAX_CACHED_ORCHESTRATOR_CONTENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_PROMOTED_SKILLS: usize = 16;

pub(crate) struct SkillsThreadState {
    config: Mutex<SkillsExtensionConfig>,
    orchestrator_skills_available: bool,
    promoted_skills: Mutex<Vec<PromotedSkillIdentity>>,
    projected_promoted_skills: Mutex<Vec<PromotedSkillIdentity>>,
    pub(crate) host_snapshot: Mutex<Option<Arc<HostSkillsSnapshot>>>,
    executor_cache: Mutex<Vec<CachedExecutorCatalog>>,
    orchestrator_cache: Mutex<Option<Arc<OrchestratorGenerationCache>>>,
}

impl SkillsThreadState {
    pub(crate) fn new(config: SkillsExtensionConfig, orchestrator_skills_available: bool) -> Self {
        Self {
            config: Mutex::new(config),
            orchestrator_skills_available,
            promoted_skills: Mutex::new(Vec::new()),
            projected_promoted_skills: Mutex::new(Vec::new()),
            host_snapshot: Mutex::new(None),
            executor_cache: Mutex::new(Vec::new()),
            orchestrator_cache: Mutex::new(None),
        }
    }

    pub(crate) fn config(&self) -> SkillsExtensionConfig {
        self.config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set_config(&self, config: SkillsExtensionConfig) {
        *self
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = config;
    }

    pub(crate) fn orchestrator_skills_enabled(&self) -> bool {
        self.orchestrator_skills_available && self.config().orchestrator_skills_enabled
    }

    pub(crate) fn restore_promoted_skills(&self, history: &ConversationHistory) {
        let promoted = history
            .items()
            .iter()
            .rev()
            .find_map(|item| {
                let ResponseItem::Message { role, content, .. } = item else {
                    return None;
                };
                if role != "developer" {
                    return None;
                }
                content.iter().find_map(|content| {
                    let ContentItem::InputText { text } = content else {
                        return None;
                    };
                    AvailableSkillsInstructions::promoted_from_rendered(text)
                })
            })
            .unwrap_or_default();
        *self
            .promoted_skills
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = promoted;
        self.projected_promoted_skills
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    pub(crate) fn resolve_promoted_skills(
        &self,
        catalog: &SkillCatalog,
    ) -> (Vec<SkillCatalogEntry>, usize) {
        let promoted = self
            .promoted_skills
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let resolved = promoted
            .iter()
            .filter_map(|identity| {
                catalog
                    .entries
                    .iter()
                    .find(|entry| entry.enabled && identity.matches_entry(entry))
                    .cloned()
            })
            .collect::<Vec<_>>();
        let omitted = promoted.len().saturating_sub(resolved.len());
        (resolved, omitted)
    }

    pub(crate) fn promoted_skill_identities(&self) -> Vec<PromotedSkillIdentity> {
        self.promoted_skills
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn promoted_with(
        &self,
        entries: &[SkillCatalogEntry],
    ) -> (Vec<PromotedSkillIdentity>, bool, usize) {
        let promoted = self
            .promoted_skills
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next = promoted.clone();
        let mut omitted = 0usize;
        for entry in entries {
            let Some(identity) = PromotedSkillIdentity::from_entry(entry) else {
                omitted = omitted.saturating_add(1);
                continue;
            };
            if !next.contains(&identity) {
                if next.len() == MAX_PROMOTED_SKILLS {
                    omitted = omitted.saturating_add(1);
                    continue;
                }
                next.push(identity);
                if !promoted_metadata_is_bounded(&next) {
                    next.pop();
                    omitted = omitted.saturating_add(1);
                }
            }
        }
        let changed = next != *promoted;
        (next, changed, omitted)
    }

    pub(crate) fn promoted_projection_changed(&self, entries: &[SkillCatalogEntry]) -> bool {
        let projected = entries
            .iter()
            .filter_map(PromotedSkillIdentity::from_entry)
            .collect::<Vec<_>>();
        projected
            != *self
                .projected_promoted_skills
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn acknowledge_promoted_skills(
        &self,
        promoted: Vec<PromotedSkillIdentity>,
        projected_entries: &[SkillCatalogEntry],
    ) {
        *self
            .promoted_skills
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = promoted;
        *self
            .projected_promoted_skills
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = projected_entries
            .iter()
            .filter_map(PromotedSkillIdentity::from_entry)
            .collect();
    }

    pub(crate) fn acknowledge_promoted_projection(&self, entries: &[SkillCatalogEntry]) {
        *self
            .projected_promoted_skills
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = entries
            .iter()
            .filter_map(PromotedSkillIdentity::from_entry)
            .collect();
    }

    /// Returns catalogs for stable selected roots.
    ///
    /// The first catalog returned for a root remains cached until this thread state is dropped.
    /// Environment availability only controls whether the root is projected into the current
    /// step; it never invalidates the cache. There is intentionally no filesystem watcher or
    /// content-based invalidation because selected environment roots are treated as stable.
    #[tracing::instrument(
        name = "skills.executor.catalog_snapshot",
        level = "info",
        skip_all,
        fields(root_count = query.executor_roots.len())
    )]
    pub(crate) async fn executor_catalog_snapshot(
        &self,
        providers: &SkillProviders,
        mut query: SkillListQuery,
    ) -> SkillCatalog {
        let roots = std::mem::take(&mut query.executor_roots);
        let mut catalog = SkillCatalog::default();
        for root in roots {
            query.executor_roots = vec![root.clone()];
            catalog.extend(
                self.executor_root_catalog(providers, root, query.clone())
                    .await,
            );
        }
        catalog
    }

    pub(crate) async fn orchestrator_catalog_snapshot(
        &self,
        mcp_resources: Option<&McpResourceClient>,
        initialize: impl Future<Output = Result<SkillCatalog, SkillProviderError>> + Send,
    ) -> SkillCatalog {
        match self
            .orchestrator_cache(mcp_resources)
            .catalog
            .get_or_try_init(|| initialize)
            .await
        {
            Ok(catalog) => catalog.clone(),
            Err(err) => SkillCatalog {
                warnings: vec![err.message],
                ..Default::default()
            },
        }
    }

    pub(crate) async fn read_skill(
        &self,
        providers: &SkillProviders,
        request: SkillReadRequest,
    ) -> SkillProviderResult<SkillReadResult> {
        if request.authority.kind != SkillSourceKind::Orchestrator {
            return providers.read(request).await;
        }

        let cache = self.orchestrator_cache(request.mcp_resources.as_deref());
        let cache_key = SkillReadCacheKey::from(&request);
        if let Some(result) = cache
            .resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&cache_key)
        {
            return Ok(result);
        }

        let result = providers.read(request).await?;
        if result.resource != cache_key.resource {
            return Ok(result);
        }

        Ok(cache
            .resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(cache_key, result))
    }

    fn orchestrator_cache(
        &self,
        mcp_resources: Option<&McpResourceClient>,
    ) -> Arc<OrchestratorGenerationCache> {
        let mut cache = self
            .orchestrator_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cache_key = mcp_resources.map(McpResourceClient::cache_key);
        if let Some(cache) = cache
            .as_ref()
            .filter(|cache| cache.mcp_cache_key == cache_key)
        {
            return Arc::clone(cache);
        }

        let next_cache = Arc::new(OrchestratorGenerationCache {
            mcp_cache_key: cache_key,
            catalog: OnceCell::new(),
            resources: Mutex::new(OrchestratorResourceCache::default()),
        });
        *cache = Some(Arc::clone(&next_cache));
        next_cache
    }

    #[tracing::instrument(name = "skills.executor.catalog_root", level = "info", skip_all)]
    async fn executor_root_catalog(
        &self,
        providers: &SkillProviders,
        root: SelectedCapabilityRoot,
        query: SkillListQuery,
    ) -> SkillCatalog {
        if let Some(cached) = self
            .executor_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|cached| cached.root == root)
        {
            return cached.catalog.clone();
        }

        let discovered = providers.list_executor_for_turn(query).await;
        let mut cache = self
            .executor_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = cache.iter().find(|cached| cached.root == root) {
            return cached.catalog.clone();
        }
        cache.push(CachedExecutorCatalog {
            root,
            catalog: discovered.clone(),
        });
        discovered
    }
}

struct CachedExecutorCatalog {
    root: SelectedCapabilityRoot,
    catalog: SkillCatalog,
}

struct OrchestratorGenerationCache {
    mcp_cache_key: Option<McpResourceClientCacheKey>,
    catalog: OnceCell<SkillCatalog>,
    resources: Mutex<OrchestratorResourceCache>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SkillReadCacheKey {
    authority: SkillAuthority,
    package: SkillPackageId,
    resource: SkillResourceId,
}

impl From<&SkillReadRequest> for SkillReadCacheKey {
    fn from(request: &SkillReadRequest) -> Self {
        Self {
            authority: request.authority.clone(),
            package: request.package.clone(),
            resource: request.resource.clone(),
        }
    }
}

#[derive(Default)]
struct OrchestratorResourceCache {
    entries: HashMap<SkillReadCacheKey, SkillReadResult>,
    contents_bytes: usize,
}

impl OrchestratorResourceCache {
    fn get(&self, key: &SkillReadCacheKey) -> Option<SkillReadResult> {
        self.entries.get(key).cloned()
    }

    fn insert(&mut self, key: SkillReadCacheKey, result: SkillReadResult) -> SkillReadResult {
        if let Some(cached) = self.entries.get(&key) {
            return cached.clone();
        }

        let contents_bytes = result.contents.len();
        let Some(next_contents_bytes) = self.contents_bytes.checked_add(contents_bytes) else {
            return result;
        };
        if self.entries.len() >= MAX_CACHED_ORCHESTRATOR_RESOURCES
            || next_contents_bytes > MAX_CACHED_ORCHESTRATOR_CONTENT_BYTES
        {
            return result;
        }

        self.contents_bytes = next_contents_bytes;
        self.entries.insert(key, result.clone());
        result
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ExecutorSkillsStepState(pub(crate) SkillCatalog);
