use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use lru::LruCache;
use orion_error::{ToStructError, UvsFrom};
use wp_error::{KnowledgeReason, KnowledgeResult};
use wp_log::{debug_kdb, warn_kdb};
use wp_model_core::model::{DataField, DataType, Value};

use crate::loader::ProviderKind;
use crate::mem::RowData;
use crate::telemetry::{
    CacheLayer, CacheOutcome, CacheTelemetryEvent, QueryTelemetryEvent, ReloadOutcome,
    ReloadTelemetryEvent, telemetry,
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

pub trait ProviderExecutor: Send + Sync {
    fn query(&self, sql: &str) -> KnowledgeResult<Vec<RowData>>;
    fn query_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<Vec<RowData>>;
    fn query_row(&self, sql: &str) -> KnowledgeResult<RowData>;
    fn query_named_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<RowData>;
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
    result_cache_config: RwLock<ResultCacheConfig>,
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
            result_cache_config: RwLock::new(config),
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
                telemetry().on_reload(&ReloadTelemetryEvent {
                    outcome: ReloadOutcome::Failure,
                    provider_kind: kind.clone(),
                });
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
        {
            let mut guard = self
                .provider
                .write()
                .expect("runtime provider lock poisoned");
            *guard = Some(handle);
        }
        self.reload_successes.fetch_add(1, Ordering::Relaxed);
        telemetry().on_reload(&ReloadTelemetryEvent {
            outcome: ReloadOutcome::Success,
            provider_kind: kind.clone(),
        });
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
        self.provider
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().map(|handle| handle.generation))
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
        let result_cache_config = self
            .result_cache_config
            .read()
            .map(|guard| *guard)
            .unwrap_or_default();
        let use_global_cache =
            matches!(req.cache_policy, CachePolicy::UseGlobal) && result_cache_config.enabled;
        if use_global_cache && let Some(hit) = self.fetch_result_cache(&handle, req) {
            self.record_result_cache_hit();
            telemetry().on_cache(&CacheTelemetryEvent {
                layer: CacheLayer::Result,
                outcome: CacheOutcome::Hit,
                provider_kind: Some(handle.kind.clone()),
            });
            debug_kdb!(
                "[kdb] global result cache hit kind={:?} generation={}",
                handle.kind,
                handle.generation.0
            );
            return Ok(hit);
        }
        if use_global_cache {
            self.record_result_cache_miss();
            telemetry().on_cache(&CacheTelemetryEvent {
                layer: CacheLayer::Result,
                outcome: CacheOutcome::Miss,
                provider_kind: Some(handle.kind.clone()),
            });
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
                telemetry().on_query(&QueryTelemetryEvent {
                    provider_kind: handle.kind.clone(),
                    mode: mode_tag,
                    success: true,
                    elapsed: started.elapsed(),
                });
                response
            }
            Err(err) => {
                telemetry().on_query(&QueryTelemetryEvent {
                    provider_kind: handle.kind.clone(),
                    mode: mode_tag,
                    success: false,
                    elapsed: started.elapsed(),
                });
                return Err(err);
            }
        };

        if use_global_cache {
            self.save_result_cache(&handle, req, response.clone());
            debug_kdb!(
                "[kdb] global result cache store kind={:?} generation={}",
                handle.kind,
                handle.generation.0
            );
        }

        Ok(response)
    }

    fn current_handle(&self) -> KnowledgeResult<Arc<ProviderHandle>> {
        self.provider
            .read()
            .expect("runtime provider lock poisoned")
            .clone()
            .ok_or_else(|| {
                KnowledgeReason::from_logic()
                    .to_err()
                    .with_detail("knowledge provider not initialized")
            })
    }

    fn fetch_result_cache(
        &self,
        handle: &ProviderHandle,
        req: &QueryRequest,
    ) -> Option<QueryResponse> {
        let config = self
            .result_cache_config
            .read()
            .map(|guard| *guard)
            .unwrap_or_default();
        if !config.enabled {
            return None;
        }
        let key = result_cache_key(handle, req);
        let cached = self
            .result_cache
            .read()
            .ok()
            .and_then(|cache| cache.peek(&key).cloned())?;
        if cached.cached_at.elapsed() > config.ttl {
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
        if let Ok(mut cache) = self.result_cache.write() {
            cache.put(
                result_cache_key(handle, req),
                CachedQueryResponse {
                    response: Arc::new(response),
                    cached_at: Instant::now(),
                },
            );
        }
    }
}

pub fn runtime() -> &'static KnowledgeRuntime {
    static RUNTIME: OnceLock<KnowledgeRuntime> = OnceLock::new();
    RUNTIME.get_or_init(|| KnowledgeRuntime::new(1024))
}

#[cfg(test)]
pub(crate) fn runtime_test_guard() -> &'static std::sync::Mutex<()> {
    static GUARD: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| std::sync::Mutex::new(()))
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
    use wp_model_core::model::Value;

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
}
