use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use crate::error::{KnowledgeResult, Reason};
use async_trait::async_trait;
use lru::LruCache;
use orion_error::UvsFrom;
use orion_error::conversion::ToStructError;
use tokio::task;
use wp_log::{debug_kdb, warn_kdb};
use wp_model_core::model::{DataField, DataType, Value};

use crate::loader::ProviderKind;
use crate::mem::RowData;
use crate::telemetry::{
    CacheLayer, CacheOutcome, CacheTelemetryEvent, QueryTelemetryEvent, ReloadOutcome,
    ReloadTelemetryEvent, telemetry, telemetry_enabled,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DatasourceId(pub String);

impl DatasourceId {
    pub fn from_seed(kind: ProviderKind, seed: &str) -> Self {
        let mut hasher = DefaultHasher::new();
        seed.hash(&mut hasher);
        let kind_str = match kind {
            ProviderKind::SqliteAuthority => "sqlite",
            ProviderKind::Postgres => "postgres",
            ProviderKind::Mysql => "mysql",
        };
        Self(format!("{kind_str}:{:016x}", hasher.finish()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Generation(pub u64);

#[derive(Debug, Clone)]
pub enum QueryMode {
    Many,
    FirstRow,
}

#[derive(Debug, Clone, Copy)]
pub enum CachePolicy {
    Bypass,
    UseGlobal,
    UseCallScope,
}

#[derive(Debug, Clone)]
pub enum QueryValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

#[derive(Debug, Clone)]
pub struct QueryParam {
    pub name: String,
    pub value: QueryValue,
}

#[derive(Debug, Clone)]
pub struct QueryRequest {
    pub sql: String,
    pub params: Vec<QueryParam>,
    pub mode: QueryMode,
    pub cache_policy: CachePolicy,
}

impl QueryRequest {
    pub fn many(
        sql: impl Into<String>,
        params: Vec<QueryParam>,
        cache_policy: CachePolicy,
    ) -> Self {
        Self {
            sql: sql.into(),
            params,
            mode: QueryMode::Many,
            cache_policy,
        }
    }

    pub fn first_row(
        sql: impl Into<String>,
        params: Vec<QueryParam>,
        cache_policy: CachePolicy,
    ) -> Self {
        Self {
            sql: sql.into(),
            params,
            mode: QueryMode::FirstRow,
            cache_policy,
        }
    }
}

#[derive(Debug, Clone)]
pub enum QueryResponse {
    Rows(Vec<RowData>),
    Row(RowData),
}

impl QueryResponse {
    pub fn into_rows(self) -> Vec<RowData> {
        match self {
            QueryResponse::Rows(rows) => rows,
            QueryResponse::Row(row) => vec![row],
        }
    }

    pub fn into_row(self) -> RowData {
        match self {
            QueryResponse::Rows(rows) => rows.into_iter().next().unwrap_or_default(),
            QueryResponse::Row(row) => row,
        }
    }
}

#[async_trait]
pub trait ProviderExecutor: Send + Sync {
    fn query(&self, sql: &str) -> KnowledgeResult<Vec<RowData>>;
    fn query_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<Vec<RowData>>;
    fn query_row(&self, sql: &str) -> KnowledgeResult<RowData>;
    fn query_named_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<RowData>;

    async fn query_async(&self, sql: &str) -> KnowledgeResult<Vec<RowData>> {
        self.query(sql)
    }

    async fn query_fields_async(
        &self,
        sql: &str,
        params: &[DataField],
    ) -> KnowledgeResult<Vec<RowData>> {
        self.query_fields(sql, params)
    }

    async fn query_row_async(&self, sql: &str) -> KnowledgeResult<RowData> {
        self.query_row(sql)
    }

    async fn query_named_fields_async(
        &self,
        sql: &str,
        params: &[DataField],
    ) -> KnowledgeResult<RowData> {
        self.query_named_fields(sql, params)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueryModeTag {
    Many,
    FirstRow,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResultCacheKey {
    pub datasource_id: DatasourceId,
    pub generation: Generation,
    pub query_hash: u64,
    pub params_hash: u64,
    pub mode: QueryModeTag,
}

pub struct ProviderHandle {
    pub provider: Arc<dyn ProviderExecutor>,
    pub datasource_id: DatasourceId,
    pub generation: Generation,
    pub kind: ProviderKind,
}

#[derive(Debug, Clone)]
pub struct RuntimeSnapshot {
    pub provider_kind: Option<ProviderKind>,
    pub datasource_id: Option<DatasourceId>,
    pub generation: Option<Generation>,
    pub result_cache_enabled: bool,
    pub result_cache_len: usize,
    pub result_cache_capacity: usize,
    pub result_cache_ttl_ms: u64,
    pub metadata_cache_len: usize,
    pub metadata_cache_capacity: usize,
    pub result_cache_hits: u64,
    pub result_cache_misses: u64,
    pub metadata_cache_hits: u64,
    pub metadata_cache_misses: u64,
    pub local_cache_hits: u64,
    pub local_cache_misses: u64,
    pub reload_successes: u64,
    pub reload_failures: u64,
}

#[derive(Debug, Clone)]
pub struct MetadataCacheScope {
    pub datasource_id: DatasourceId,
    pub generation: Generation,
}

#[derive(Debug, Clone, Copy)]
pub struct ResultCacheConfig {
    pub enabled: bool,
    pub capacity: usize,
    pub ttl: Duration,
}

impl Default for ResultCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            capacity: 1024,
            ttl: Duration::from_millis(30_000),
        }
    }
}

#[derive(Debug, Clone)]
struct CachedQueryResponse {
    response: Arc<QueryResponse>,
    cached_at: Instant,
}

pub struct KnowledgeRuntime {
    provider: RwLock<Option<Arc<ProviderHandle>>>,
    next_generation: AtomicU64,
    provider_epoch: AtomicU64,
    current_generation_value: AtomicU64,
    result_cache_config: RwLock<ResultCacheConfig>,
    result_cache_enabled: AtomicBool,
    result_cache_ttl_ms: AtomicU64,
    result_cache: RwLock<LruCache<ResultCacheKey, CachedQueryResponse>>,
    result_cache_hits: AtomicU64,
    result_cache_misses: AtomicU64,
    metadata_cache_hits: AtomicU64,
    metadata_cache_misses: AtomicU64,
    local_cache_hits: AtomicU64,
    local_cache_misses: AtomicU64,
    reload_successes: AtomicU64,
    reload_failures: AtomicU64,
}

impl KnowledgeRuntime {
    pub fn new(result_cache_capacity: usize) -> Self {
        let config = ResultCacheConfig {
            capacity: result_cache_capacity.max(1),
            ..ResultCacheConfig::default()
        };
        let capacity = NonZeroUsize::new(config.capacity).expect("non-zero capacity");
        Self {
            provider: RwLock::new(None),
            next_generation: AtomicU64::new(0),
            provider_epoch: AtomicU64::new(0),
            current_generation_value: AtomicU64::new(0),
            result_cache_config: RwLock::new(config),
            result_cache_enabled: AtomicBool::new(config.enabled),
            result_cache_ttl_ms: AtomicU64::new(config.ttl.as_millis() as u64),
            result_cache: RwLock::new(LruCache::new(capacity)),
            result_cache_hits: AtomicU64::new(0),
            result_cache_misses: AtomicU64::new(0),
            metadata_cache_hits: AtomicU64::new(0),
            metadata_cache_misses: AtomicU64::new(0),
            local_cache_hits: AtomicU64::new(0),
            local_cache_misses: AtomicU64::new(0),
            reload_successes: AtomicU64::new(0),
            reload_failures: AtomicU64::new(0),
        }
    }

    pub fn install_provider<F>(
        &self,
        kind: ProviderKind,
        datasource_id: DatasourceId,
        build: F,
    ) -> KnowledgeResult<Generation>
    where
        F: FnOnce(Generation) -> KnowledgeResult<Arc<dyn ProviderExecutor>>,
    {
        let generation = Generation(self.next_generation.fetch_add(1, Ordering::SeqCst) + 1);
        let previous = self
            .provider
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().cloned());
        debug_kdb!(
            "[kdb] reload provider start kind={kind:?} datasource_id={} target_generation={} previous_generation={}",
            datasource_id.0,
            generation.0,
            previous
                .as_ref()
                .map(|handle| handle.generation.0.to_string())
                .unwrap_or_else(|| "none".to_string())
        );
        let provider = match build(generation) {
            Ok(provider) => provider,
            Err(err) => {
                self.reload_failures.fetch_add(1, Ordering::Relaxed);
                warn_kdb!(
                    "[kdb] reload provider failed kind={kind:?} datasource_id={} target_generation={} err={}",
                    datasource_id.0,
                    generation.0,
                    err
                );
                if telemetry_enabled() {
                    telemetry().on_reload(&ReloadTelemetryEvent {
                        outcome: ReloadOutcome::Failure,
                        provider_kind: kind.clone(),
                    });
                }
                return Err(err);
            }
        };
        debug_kdb!(
            "[kdb] install provider kind={kind:?} datasource_id={} generation={}",
            datasource_id.0,
            generation.0
        );
        let kind_for_handle = kind.clone();
        let datasource_id_for_handle = datasource_id.clone();
        let handle = Arc::new(ProviderHandle {
            provider,
            datasource_id: datasource_id_for_handle,
            generation,
            kind: kind_for_handle,
        });
        self.provider_epoch.fetch_add(1, Ordering::AcqRel);
        {
            let mut guard = self
                .provider
                .write()
                .expect("runtime provider lock poisoned");
            *guard = Some(handle);
        }
        self.current_generation_value
            .store(generation.0, Ordering::Release);
        self.provider_epoch.fetch_add(1, Ordering::Release);
        self.reload_successes.fetch_add(1, Ordering::Relaxed);
        if telemetry_enabled() {
            telemetry().on_reload(&ReloadTelemetryEvent {
                outcome: ReloadOutcome::Success,
                provider_kind: kind.clone(),
            });
        }
        debug_kdb!(
            "[kdb] reload provider success kind={kind:?} datasource_id={} generation={}",
            datasource_id.0,
            generation.0
        );
        Ok(generation)
    }

    pub fn configure_result_cache(&self, enabled: bool, capacity: usize, ttl: Duration) {
        let new_config = ResultCacheConfig {
            enabled,
            capacity: capacity.max(1),
            ttl: ttl.max(Duration::from_millis(1)),
        };
        let mut should_reset_cache = false;
        {
            let mut guard = self
                .result_cache_config
                .write()
                .expect("runtime result cache config lock poisoned");
            if guard.capacity != new_config.capacity || (!new_config.enabled && guard.enabled) {
                should_reset_cache = true;
            }
            *guard = new_config;
        }
        self.result_cache_enabled
            .store(new_config.enabled, Ordering::Relaxed);
        self.result_cache_ttl_ms.store(
            new_config.ttl.as_millis().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );

        if should_reset_cache {
            let mut cache = self
                .result_cache
                .write()
                .expect("runtime result cache lock poisoned");
            *cache = LruCache::new(
                NonZeroUsize::new(new_config.capacity).expect("non-zero result cache capacity"),
            );
        }
    }

    pub fn current_generation(&self) -> Option<Generation> {
        let epoch_before = self.provider_epoch.load(Ordering::Acquire);
        if epoch_before % 2 == 1 {
            return self.current_generation_from_provider();
        }
        let generation = self.current_generation_value.load(Ordering::Acquire);
        let epoch_after = self.provider_epoch.load(Ordering::Acquire);
        if epoch_before != epoch_after {
            return self.current_generation_from_provider();
        }
        match generation {
            0 => None,
            generation => Some(Generation(generation)),
        }
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        let provider = self
            .provider
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().cloned());
        let result_cache_config = self
            .result_cache_config
            .read()
            .map(|guard| *guard)
            .unwrap_or_default();
        let (result_cache_len, result_cache_capacity) = self
            .result_cache
            .read()
            .map(|cache| (cache.len(), cache.cap().get()))
            .unwrap_or((0, 0));
        let (metadata_cache_len, metadata_cache_capacity) =
            crate::mem::query_util::column_metadata_cache_snapshot();
        RuntimeSnapshot {
            provider_kind: provider.as_ref().map(|handle| handle.kind.clone()),
            datasource_id: provider.as_ref().map(|handle| handle.datasource_id.clone()),
            generation: provider.as_ref().map(|handle| handle.generation),
            result_cache_enabled: result_cache_config.enabled,
            result_cache_len,
            result_cache_capacity,
            result_cache_ttl_ms: result_cache_config.ttl.as_millis() as u64,
            metadata_cache_len,
            metadata_cache_capacity,
            result_cache_hits: self.result_cache_hits.load(Ordering::Relaxed),
            result_cache_misses: self.result_cache_misses.load(Ordering::Relaxed),
            metadata_cache_hits: self.metadata_cache_hits.load(Ordering::Relaxed),
            metadata_cache_misses: self.metadata_cache_misses.load(Ordering::Relaxed),
            local_cache_hits: self.local_cache_hits.load(Ordering::Relaxed),
            local_cache_misses: self.local_cache_misses.load(Ordering::Relaxed),
            reload_successes: self.reload_successes.load(Ordering::Relaxed),
            reload_failures: self.reload_failures.load(Ordering::Relaxed),
        }
    }

    pub fn current_metadata_scope(&self) -> MetadataCacheScope {
        self.provider
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().cloned())
            .map(|handle| MetadataCacheScope {
                datasource_id: handle.datasource_id.clone(),
                generation: handle.generation,
            })
            .unwrap_or_else(|| MetadataCacheScope {
                datasource_id: DatasourceId("sqlite:standalone".to_string()),
                generation: Generation(0),
            })
    }

    pub fn current_provider_kind(&self) -> Option<ProviderKind> {
        self.provider
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().map(|handle| handle.kind.clone()))
    }

    pub fn record_result_cache_hit(&self) {
        self.result_cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_result_cache_miss(&self) {
        self.result_cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_metadata_cache_hit(&self) {
        self.metadata_cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_metadata_cache_miss(&self) {
        self.metadata_cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_local_cache_hit(&self) {
        self.local_cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_local_cache_miss(&self) {
        self.local_cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn execute(&self, req: &QueryRequest) -> KnowledgeResult<QueryResponse> {
        let handle = self.current_handle()?;
        self.execute_with_handle(&handle, req)
    }

    fn execute_with_handle(
        &self,
        handle: &Arc<ProviderHandle>,
        req: &QueryRequest,
    ) -> KnowledgeResult<QueryResponse> {
        let use_global_cache =
            matches!(req.cache_policy, CachePolicy::UseGlobal) && self.result_cache_enabled();
        if use_global_cache && let Some(hit) = self.fetch_result_cache(handle, req) {
            self.record_result_cache_hit();
            if telemetry_enabled() {
                telemetry().on_cache(&CacheTelemetryEvent {
                    layer: CacheLayer::Result,
                    outcome: CacheOutcome::Hit,
                    provider_kind: Some(handle.kind.clone()),
                });
            }
            debug_kdb!(
                "[kdb] global result cache hit kind={:?} generation={}",
                handle.kind,
                handle.generation.0
            );
            return Ok(hit);
        }
        if use_global_cache {
            self.record_result_cache_miss();
            if telemetry_enabled() {
                telemetry().on_cache(&CacheTelemetryEvent {
                    layer: CacheLayer::Result,
                    outcome: CacheOutcome::Miss,
                    provider_kind: Some(handle.kind.clone()),
                });
            }
            debug_kdb!(
                "[kdb] global result cache miss kind={:?} generation={}",
                handle.kind,
                handle.generation.0
            );
        }

        let params = params_to_fields(&req.params);
        let mode_tag = query_mode_tag(&req.mode);
        let started = Instant::now();
        debug_kdb!(
            "[kdb] execute query kind={:?} generation={} mode={:?} cache_policy={:?}",
            handle.kind,
            handle.generation.0,
            req.mode,
            req.cache_policy
        );
        let response = match match req.mode {
            QueryMode::Many => {
                if params.is_empty() {
                    handle.provider.query(&req.sql).map(QueryResponse::Rows)
                } else {
                    handle
                        .provider
                        .query_fields(&req.sql, &params)
                        .map(QueryResponse::Rows)
                }
            }
            QueryMode::FirstRow => {
                if params.is_empty() {
                    handle.provider.query_row(&req.sql).map(QueryResponse::Row)
                } else {
                    handle
                        .provider
                        .query_named_fields(&req.sql, &params)
                        .map(QueryResponse::Row)
                }
            }
        } {
            Ok(response) => {
                if telemetry_enabled() {
                    telemetry().on_query(&QueryTelemetryEvent {
                        provider_kind: handle.kind.clone(),
                        mode: mode_tag,
                        success: true,
                        elapsed: started.elapsed(),
                    });
                }
                response
            }
            Err(err) => {
                if telemetry_enabled() {
                    telemetry().on_query(&QueryTelemetryEvent {
                        provider_kind: handle.kind.clone(),
                        mode: mode_tag,
                        success: false,
                        elapsed: started.elapsed(),
                    });
                }
                return Err(err);
            }
        };

        if use_global_cache {
            self.save_result_cache(handle, req, response.clone());
            debug_kdb!(
                "[kdb] global result cache store kind={:?} generation={}",
                handle.kind,
                handle.generation.0
            );
        }

        Ok(response)
    }

    pub fn execute_first_row_fields(
        &self,
        sql: &str,
        params: &[DataField],
        cache_policy: CachePolicy,
    ) -> KnowledgeResult<RowData> {
        let handle = self.current_handle()?;
        self.execute_first_row_fields_with_handle(&handle, sql, params, cache_policy)
    }

    fn execute_first_row_fields_with_handle(
        &self,
        handle: &Arc<ProviderHandle>,
        sql: &str,
        params: &[DataField],
        cache_policy: CachePolicy,
    ) -> KnowledgeResult<RowData> {
        let use_global_cache =
            matches!(cache_policy, CachePolicy::UseGlobal) && self.result_cache_enabled();
        if use_global_cache
            && let Some(hit) = self.fetch_result_cache_by_key(result_cache_key_fields(
                handle,
                sql,
                params,
                QueryModeTag::FirstRow,
            ))
        {
            self.record_result_cache_hit();
            if telemetry_enabled() {
                telemetry().on_cache(&CacheTelemetryEvent {
                    layer: CacheLayer::Result,
                    outcome: CacheOutcome::Hit,
                    provider_kind: Some(handle.kind.clone()),
                });
            }
            return Ok(hit.into_row());
        }
        if use_global_cache {
            self.record_result_cache_miss();
            if telemetry_enabled() {
                telemetry().on_cache(&CacheTelemetryEvent {
                    layer: CacheLayer::Result,
                    outcome: CacheOutcome::Miss,
                    provider_kind: Some(handle.kind.clone()),
                });
            }
        }

        let started = Instant::now();
        let row = if params.is_empty() {
            handle.provider.query_row(sql)
        } else {
            handle.provider.query_named_fields(sql, params)
        };
        let row = match row {
            Ok(row) => {
                if telemetry_enabled() {
                    telemetry().on_query(&QueryTelemetryEvent {
                        provider_kind: handle.kind.clone(),
                        mode: QueryModeTag::FirstRow,
                        success: true,
                        elapsed: started.elapsed(),
                    });
                }
                row
            }
            Err(err) => {
                if telemetry_enabled() {
                    telemetry().on_query(&QueryTelemetryEvent {
                        provider_kind: handle.kind.clone(),
                        mode: QueryModeTag::FirstRow,
                        success: false,
                        elapsed: started.elapsed(),
                    });
                }
                return Err(err);
            }
        };

        if use_global_cache {
            self.save_result_cache_by_key(
                result_cache_key_fields(handle, sql, params, QueryModeTag::FirstRow),
                QueryResponse::Row(row.clone()),
            );
        }

        Ok(row)
    }

    pub async fn execute_async(&self, req: &QueryRequest) -> KnowledgeResult<QueryResponse> {
        let handle = self.current_handle()?;
        if matches!(handle.kind, ProviderKind::SqliteAuthority) {
            let handle = handle.clone();
            let req = req.clone();
            return task::spawn_blocking(move || runtime().execute_with_handle(&handle, &req))
                .await
                .map_err(|err| {
                    Reason::from_logic()
                        .to_err()
                        .with_detail(format!("knowledge async sqlite query join failed: {err}"))
                })?;
        }
        let use_global_cache =
            matches!(req.cache_policy, CachePolicy::UseGlobal) && self.result_cache_enabled();
        if use_global_cache && let Some(hit) = self.fetch_result_cache(&handle, req) {
            self.record_result_cache_hit();
            if telemetry_enabled() {
                telemetry().on_cache(&CacheTelemetryEvent {
                    layer: CacheLayer::Result,
                    outcome: CacheOutcome::Hit,
                    provider_kind: Some(handle.kind.clone()),
                });
            }
            return Ok(hit);
        }
        if use_global_cache {
            self.record_result_cache_miss();
            if telemetry_enabled() {
                telemetry().on_cache(&CacheTelemetryEvent {
                    layer: CacheLayer::Result,
                    outcome: CacheOutcome::Miss,
                    provider_kind: Some(handle.kind.clone()),
                });
            }
        }

        let params = params_to_fields(&req.params);
        let mode_tag = query_mode_tag(&req.mode);
        let started = Instant::now();
        let response = match req.mode {
            QueryMode::Many => {
                if params.is_empty() {
                    handle
                        .provider
                        .query_async(&req.sql)
                        .await
                        .map(QueryResponse::Rows)
                } else {
                    handle
                        .provider
                        .query_fields_async(&req.sql, &params)
                        .await
                        .map(QueryResponse::Rows)
                }
            }
            QueryMode::FirstRow => {
                if params.is_empty() {
                    handle
                        .provider
                        .query_row_async(&req.sql)
                        .await
                        .map(QueryResponse::Row)
                } else {
                    handle
                        .provider
                        .query_named_fields_async(&req.sql, &params)
                        .await
                        .map(QueryResponse::Row)
                }
            }
        };
        let response = match response {
            Ok(response) => {
                if telemetry_enabled() {
                    telemetry().on_query(&QueryTelemetryEvent {
                        provider_kind: handle.kind.clone(),
                        mode: mode_tag,
                        success: true,
                        elapsed: started.elapsed(),
                    });
                }
                response
            }
            Err(err) => {
                if telemetry_enabled() {
                    telemetry().on_query(&QueryTelemetryEvent {
                        provider_kind: handle.kind.clone(),
                        mode: mode_tag,
                        success: false,
                        elapsed: started.elapsed(),
                    });
                }
                return Err(err);
            }
        };

        if use_global_cache {
            self.save_result_cache(&handle, req, response.clone());
        }

        Ok(response)
    }

    pub async fn execute_first_row_fields_async(
        &self,
        sql: &str,
        params: &[DataField],
        cache_policy: CachePolicy,
    ) -> KnowledgeResult<RowData> {
        let handle = self.current_handle()?;
        if matches!(handle.kind, ProviderKind::SqliteAuthority) {
            let handle = handle.clone();
            let sql = sql.to_string();
            let params = params.to_vec();
            return task::spawn_blocking(move || {
                runtime().execute_first_row_fields_with_handle(&handle, &sql, &params, cache_policy)
            })
            .await
            .map_err(|err| {
                Reason::from_logic().to_err().with_detail(format!(
                    "knowledge async sqlite first-row query join failed: {err}"
                ))
            })?;
        }
        let use_global_cache =
            matches!(cache_policy, CachePolicy::UseGlobal) && self.result_cache_enabled();
        if use_global_cache
            && let Some(hit) = self.fetch_result_cache_by_key(result_cache_key_fields(
                &handle,
                sql,
                params,
                QueryModeTag::FirstRow,
            ))
        {
            self.record_result_cache_hit();
            if telemetry_enabled() {
                telemetry().on_cache(&CacheTelemetryEvent {
                    layer: CacheLayer::Result,
                    outcome: CacheOutcome::Hit,
                    provider_kind: Some(handle.kind.clone()),
                });
            }
            return Ok(hit.into_row());
        }
        if use_global_cache {
            self.record_result_cache_miss();
            if telemetry_enabled() {
                telemetry().on_cache(&CacheTelemetryEvent {
                    layer: CacheLayer::Result,
                    outcome: CacheOutcome::Miss,
                    provider_kind: Some(handle.kind.clone()),
                });
            }
        }

        let started = Instant::now();
        let row = if params.is_empty() {
            handle.provider.query_row_async(sql).await
        } else {
            handle.provider.query_named_fields_async(sql, params).await
        };
        let row = match row {
            Ok(row) => {
                if telemetry_enabled() {
                    telemetry().on_query(&QueryTelemetryEvent {
                        provider_kind: handle.kind.clone(),
                        mode: QueryModeTag::FirstRow,
                        success: true,
                        elapsed: started.elapsed(),
                    });
                }
                row
            }
            Err(err) => {
                if telemetry_enabled() {
                    telemetry().on_query(&QueryTelemetryEvent {
                        provider_kind: handle.kind.clone(),
                        mode: QueryModeTag::FirstRow,
                        success: false,
                        elapsed: started.elapsed(),
                    });
                }
                return Err(err);
            }
        };

        if use_global_cache {
            self.save_result_cache_by_key(
                result_cache_key_fields(&handle, sql, params, QueryModeTag::FirstRow),
                QueryResponse::Row(row.clone()),
            );
        }

        Ok(row)
    }

    fn current_handle(&self) -> KnowledgeResult<Arc<ProviderHandle>> {
        self.provider
            .read()
            .expect("runtime provider lock poisoned")
            .clone()
            .ok_or_else(|| {
                Reason::from_logic()
                    .to_err()
                    .with_detail("knowledge provider not initialized")
            })
    }

    fn current_generation_from_provider(&self) -> Option<Generation> {
        self.provider
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().map(|handle| handle.generation))
    }

    fn fetch_result_cache(
        &self,
        handle: &ProviderHandle,
        req: &QueryRequest,
    ) -> Option<QueryResponse> {
        self.fetch_result_cache_by_key(result_cache_key(handle, req))
    }

    fn fetch_result_cache_by_key(&self, key: ResultCacheKey) -> Option<QueryResponse> {
        if !self.result_cache_enabled() {
            return None;
        }
        let cached = self
            .result_cache
            .read()
            .ok()
            .and_then(|cache| cache.peek(&key).cloned())?;
        if cached.cached_at.elapsed() > self.result_cache_ttl() {
            if let Ok(mut cache) = self.result_cache.write() {
                let _ = cache.pop(&key);
            }
            return None;
        }
        Some((*cached.response).clone())
    }

    fn save_result_cache(
        &self,
        handle: &ProviderHandle,
        req: &QueryRequest,
        response: QueryResponse,
    ) {
        self.save_result_cache_by_key(result_cache_key(handle, req), response);
    }

    fn save_result_cache_by_key(&self, key: ResultCacheKey, response: QueryResponse) {
        if let Ok(mut cache) = self.result_cache.write() {
            cache.put(
                key,
                CachedQueryResponse {
                    response: Arc::new(response),
                    cached_at: Instant::now(),
                },
            );
        }
    }

    #[inline]
    fn result_cache_enabled(&self) -> bool {
        self.result_cache_enabled.load(Ordering::Relaxed)
    }

    #[inline]
    fn result_cache_ttl(&self) -> Duration {
        Duration::from_millis(self.result_cache_ttl_ms.load(Ordering::Relaxed))
    }
}

pub fn runtime() -> &'static KnowledgeRuntime {
    static RUNTIME: OnceLock<KnowledgeRuntime> = OnceLock::new();
    RUNTIME.get_or_init(|| KnowledgeRuntime::new(1024))
}

#[cfg(test)]
pub(crate) struct RuntimeTestGuard(tokio::sync::Mutex<()>);

#[cfg(test)]
impl RuntimeTestGuard {
    pub(crate) fn lock(&self) -> Result<tokio::sync::MutexGuard<'_, ()>, std::convert::Infallible> {
        Ok(self.0.blocking_lock())
    }

    pub(crate) async fn lock_async(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.0.lock().await
    }
}

#[cfg(test)]
pub(crate) fn runtime_test_guard() -> &'static RuntimeTestGuard {
    static GUARD: OnceLock<RuntimeTestGuard> = OnceLock::new();
    GUARD.get_or_init(|| RuntimeTestGuard(tokio::sync::Mutex::new(())))
}

fn result_cache_key(handle: &ProviderHandle, req: &QueryRequest) -> ResultCacheKey {
    ResultCacheKey {
        datasource_id: handle.datasource_id.clone(),
        generation: handle.generation,
        query_hash: stable_hash(&req.sql),
        params_hash: stable_params_hash(&req.params),
        mode: match req.mode {
            QueryMode::Many => QueryModeTag::Many,
            QueryMode::FirstRow => QueryModeTag::FirstRow,
        },
    }
}

fn result_cache_key_fields(
    handle: &ProviderHandle,
    sql: &str,
    params: &[DataField],
    mode: QueryModeTag,
) -> ResultCacheKey {
    ResultCacheKey {
        datasource_id: handle.datasource_id.clone(),
        generation: handle.generation,
        query_hash: stable_hash(sql),
        params_hash: stable_field_params_hash(params),
        mode,
    }
}

fn query_mode_tag(mode: &QueryMode) -> QueryModeTag {
    match mode {
        QueryMode::Many => QueryModeTag::Many,
        QueryMode::FirstRow => QueryModeTag::FirstRow,
    }
}

fn stable_hash(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn stable_params_hash(params: &[QueryParam]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for param in params {
        param.name.hash(&mut hasher);
        match &param.value {
            QueryValue::Null => 0u8.hash(&mut hasher),
            QueryValue::Bool(value) => {
                1u8.hash(&mut hasher);
                value.hash(&mut hasher);
            }
            QueryValue::Int(value) => {
                2u8.hash(&mut hasher);
                value.hash(&mut hasher);
            }
            QueryValue::Float(value) => {
                3u8.hash(&mut hasher);
                value.to_bits().hash(&mut hasher);
            }
            QueryValue::Text(value) => {
                4u8.hash(&mut hasher);
                value.hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

fn stable_field_params_hash(params: &[DataField]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for field in params {
        field.get_name().hash(&mut hasher);
        match field.get_value() {
            Value::Null | Value::Ignore(_) => 0u8.hash(&mut hasher),
            Value::Bool(value) => {
                1u8.hash(&mut hasher);
                value.hash(&mut hasher);
            }
            Value::Digit(value) => {
                2u8.hash(&mut hasher);
                value.hash(&mut hasher);
            }
            Value::Float(value) => {
                3u8.hash(&mut hasher);
                value.to_bits().hash(&mut hasher);
            }
            Value::Chars(value) => {
                4u8.hash(&mut hasher);
                value.hash(&mut hasher);
            }
            Value::Symbol(value) => {
                5u8.hash(&mut hasher);
                value.hash(&mut hasher);
            }
            Value::Time(value) => {
                6u8.hash(&mut hasher);
                value.hash(&mut hasher);
            }
            Value::Hex(value) => {
                7u8.hash(&mut hasher);
                value.to_string().hash(&mut hasher);
            }
            Value::IpNet(value) => {
                8u8.hash(&mut hasher);
                value.to_string().hash(&mut hasher);
            }
            Value::IpAddr(value) => {
                9u8.hash(&mut hasher);
                value.hash(&mut hasher);
            }
            Value::Obj(value) => {
                10u8.hash(&mut hasher);
                format!("{:?}", value).hash(&mut hasher);
            }
            Value::Array(value) => {
                11u8.hash(&mut hasher);
                format!("{:?}", value).hash(&mut hasher);
            }
            Value::Domain(value) => {
                12u8.hash(&mut hasher);
                value.0.hash(&mut hasher);
            }
            Value::Url(value) => {
                13u8.hash(&mut hasher);
                value.0.hash(&mut hasher);
            }
            Value::Email(value) => {
                14u8.hash(&mut hasher);
                value.0.hash(&mut hasher);
            }
            Value::IdCard(value) => {
                15u8.hash(&mut hasher);
                value.0.hash(&mut hasher);
            }
            Value::MobilePhone(value) => {
                16u8.hash(&mut hasher);
                value.0.hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

pub fn fields_to_params(params: &[DataField]) -> Vec<QueryParam> {
    params
        .iter()
        .map(|field| {
            let value = match field.get_value() {
                Value::Null | Value::Ignore(_) => QueryValue::Null,
                Value::Bool(value) => QueryValue::Bool(*value),
                Value::Digit(value) => QueryValue::Int(*value),
                Value::Float(value) => QueryValue::Float(*value),
                Value::Chars(value) => QueryValue::Text(value.to_string()),
                Value::Symbol(value) => QueryValue::Text(value.to_string()),
                Value::Time(value) => QueryValue::Text(value.to_string()),
                Value::Hex(value) => QueryValue::Text(value.to_string()),
                Value::IpNet(value) => QueryValue::Text(value.to_string()),
                Value::IpAddr(value) => QueryValue::Text(value.to_string()),
                Value::Obj(value) => QueryValue::Text(format!("{:?}", value)),
                Value::Array(value) => QueryValue::Text(format!("{:?}", value)),
                Value::Domain(value) => QueryValue::Text(value.0.to_string()),
                Value::Url(value) => QueryValue::Text(value.0.to_string()),
                Value::Email(value) => QueryValue::Text(value.0.to_string()),
                Value::IdCard(value) => QueryValue::Text(value.0.to_string()),
                Value::MobilePhone(value) => QueryValue::Text(value.0.to_string()),
            };
            QueryParam {
                name: field.get_name().to_string(),
                value,
            }
        })
        .collect()
}

pub fn params_to_fields(params: &[QueryParam]) -> Vec<DataField> {
    params
        .iter()
        .map(|param| match &param.value {
            QueryValue::Null => {
                DataField::new(DataType::default(), param.name.clone(), Value::Null)
            }
            QueryValue::Bool(value) => {
                DataField::new(DataType::default(), param.name.clone(), Value::Bool(*value))
            }
            QueryValue::Int(value) => DataField::from_digit(param.name.clone(), *value),
            QueryValue::Float(value) => DataField::from_float(param.name.clone(), *value),
            QueryValue::Text(value) => DataField::from_chars(param.name.clone(), value.clone()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Arc;
    use wp_model_core::model::Value;

    struct TestProvider {
        value: &'static str,
    }

    #[async_trait]
    impl ProviderExecutor for TestProvider {
        fn query(&self, _sql: &str) -> KnowledgeResult<Vec<RowData>> {
            Ok(vec![vec![DataField::from_chars("value", self.value)]])
        }

        fn query_fields(&self, _sql: &str, _params: &[DataField]) -> KnowledgeResult<Vec<RowData>> {
            self.query("")
        }

        fn query_row(&self, _sql: &str) -> KnowledgeResult<RowData> {
            Ok(vec![DataField::from_chars("value", self.value)])
        }

        fn query_named_fields(
            &self,
            _sql: &str,
            _params: &[DataField],
        ) -> KnowledgeResult<RowData> {
            self.query_row("")
        }
    }

    #[test]
    fn query_param_hash_is_stable() {
        let params = vec![
            QueryParam {
                name: ":id".to_string(),
                value: QueryValue::Int(7),
            },
            QueryParam {
                name: ":name".to_string(),
                value: QueryValue::Text("abc".to_string()),
            },
        ];
        assert_eq!(stable_params_hash(&params), stable_params_hash(&params));
    }

    #[test]
    fn fields_to_params_preserves_raw_chars_value() {
        let fields = [DataField::from_chars(
            ":name".to_string(),
            "令狐冲".to_string(),
        )];
        let params = fields_to_params(&fields);
        assert_eq!(params.len(), 1);
        match &params[0].value {
            QueryValue::Text(value) => assert_eq!(value, "令狐冲"),
            other => panic!("unexpected param value: {other:?}"),
        }
        let roundtrip = params_to_fields(&params);
        assert!(matches!(roundtrip[0].get_value(), Value::Chars(_)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sqlite_async_bridge_keeps_captured_handle_after_reload() {
        let _guard = runtime_test_guard().lock_async().await;
        runtime()
            .install_provider(
                ProviderKind::SqliteAuthority,
                DatasourceId("sqlite:old".to_string()),
                |_generation| Ok(Arc::new(TestProvider { value: "old" })),
            )
            .expect("install old provider");
        let old_handle = runtime().current_handle().expect("current old handle");

        runtime()
            .install_provider(
                ProviderKind::SqliteAuthority,
                DatasourceId("sqlite:new".to_string()),
                |_generation| Ok(Arc::new(TestProvider { value: "new" })),
            )
            .expect("install new provider");

        let req = QueryRequest::first_row("SELECT value", Vec::new(), CachePolicy::Bypass);
        let row = task::spawn_blocking(move || runtime().execute_with_handle(&old_handle, &req))
            .await
            .expect("join sqlite bridge")
            .expect("execute old handle")
            .into_row();
        assert_eq!(row[0].to_string(), "chars(old)");
    }
}
