use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use async_trait::async_trait;
use rusqlite::ToSql;
use rusqlite::{Connection, OpenFlags};
use wp_error::KnowledgeResult;
use wp_log::{info_ctrl, warn_kdb};
use wp_model_core::model::DataField;

use crate::cache::CacheAble;
use crate::loader::{ProviderKind, parse_knowdb_conf};
use crate::mem::RowData;
use crate::mem::memdb::MemDB;
use crate::mem::thread_clone::ThreadClonedMDB;
use crate::mysql::{MySqlProvider, MySqlProviderConfig};
use crate::param::named_params_to_fields;
use crate::postgres::{PostgresProvider, PostgresProviderConfig};
use crate::runtime::{
    CachePolicy, DatasourceId, Generation, MetadataCacheScope, ProviderExecutor, QueryRequest,
    QueryResponse, RuntimeSnapshot, runtime,
};
use crate::telemetry::{
    CacheLayer, CacheOutcome, CacheTelemetryEvent, KnowledgeTelemetry, install_telemetry,
    telemetry, telemetry_enabled,
};

struct MemProvider {
    db: MemDB,
    metadata_scope: MetadataCacheScope,
}

#[async_trait]
impl ProviderExecutor for ThreadClonedMDB {
    fn query(&self, sql: &str) -> KnowledgeResult<Vec<RowData>> {
        ThreadClonedMDB::query_with_scope(self, sql)
    }

    fn query_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<Vec<RowData>> {
        ThreadClonedMDB::query_fields_with_scope(self, sql, params)
    }

    fn query_row(&self, sql: &str) -> KnowledgeResult<RowData> {
        ThreadClonedMDB::query_row_with_scope(self, sql)
    }

    fn query_named_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<RowData> {
        ThreadClonedMDB::query_named_fields_with_scope(self, sql, params)
    }
}

#[async_trait]
impl ProviderExecutor for MemProvider {
    fn query(&self, sql: &str) -> KnowledgeResult<Vec<RowData>> {
        self.db.query_with_scope(&self.metadata_scope, sql)
    }

    fn query_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<Vec<RowData>> {
        self.db
            .query_fields_with_scope(&self.metadata_scope, sql, params)
    }

    fn query_row(&self, sql: &str) -> KnowledgeResult<RowData> {
        self.db.query_row_with_scope(&self.metadata_scope, sql)
    }

    fn query_named_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<RowData> {
        self.db
            .query_named_fields_with_scope(&self.metadata_scope, sql, params)
    }
}

#[async_trait]
impl ProviderExecutor for PostgresProvider {
    fn query(&self, sql: &str) -> KnowledgeResult<Vec<RowData>> {
        PostgresProvider::query(self, sql)
    }

    fn query_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<Vec<RowData>> {
        PostgresProvider::query_fields(self, sql, params)
    }

    fn query_row(&self, sql: &str) -> KnowledgeResult<RowData> {
        PostgresProvider::query_row(self, sql)
    }

    fn query_named_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<RowData> {
        PostgresProvider::query_named_fields(self, sql, params)
    }

    async fn query_async(&self, sql: &str) -> KnowledgeResult<Vec<RowData>> {
        PostgresProvider::query_async(self, sql).await
    }

    async fn query_fields_async(
        &self,
        sql: &str,
        params: &[DataField],
    ) -> KnowledgeResult<Vec<RowData>> {
        PostgresProvider::query_fields_async(self, sql, params).await
    }

    async fn query_row_async(&self, sql: &str) -> KnowledgeResult<RowData> {
        PostgresProvider::query_row_async(self, sql).await
    }

    async fn query_named_fields_async(
        &self,
        sql: &str,
        params: &[DataField],
    ) -> KnowledgeResult<RowData> {
        PostgresProvider::query_named_fields_async(self, sql, params).await
    }
}

#[async_trait]
impl ProviderExecutor for MySqlProvider {
    fn query(&self, sql: &str) -> KnowledgeResult<Vec<RowData>> {
        MySqlProvider::query(self, sql)
    }

    fn query_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<Vec<RowData>> {
        MySqlProvider::query_fields(self, sql, params)
    }

    fn query_row(&self, sql: &str) -> KnowledgeResult<RowData> {
        MySqlProvider::query_row(self, sql)
    }

    fn query_named_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<RowData> {
        MySqlProvider::query_named_fields(self, sql, params)
    }

    async fn query_async(&self, sql: &str) -> KnowledgeResult<Vec<RowData>> {
        MySqlProvider::query_async(self, sql).await
    }

    async fn query_fields_async(
        &self,
        sql: &str,
        params: &[DataField],
    ) -> KnowledgeResult<Vec<RowData>> {
        MySqlProvider::query_fields_async(self, sql, params).await
    }

    async fn query_row_async(&self, sql: &str) -> KnowledgeResult<RowData> {
        MySqlProvider::query_row_async(self, sql).await
    }

    async fn query_named_fields_async(
        &self,
        sql: &str,
        params: &[DataField],
    ) -> KnowledgeResult<RowData> {
        MySqlProvider::query_named_fields_async(self, sql, params).await
    }
}

fn install_provider<F>(
    kind: ProviderKind,
    datasource_id: DatasourceId,
    build: F,
) -> KnowledgeResult<()>
where
    F: FnOnce(Generation) -> KnowledgeResult<Arc<dyn ProviderExecutor>>,
{
    runtime().install_provider(kind, datasource_id, build)?;
    Ok(())
}

fn datasource_id_for(kind: ProviderKind, seed: &str) -> DatasourceId {
    DatasourceId::from_seed(kind, seed)
}

pub fn init_thread_cloned_from_authority(authority_uri: &str) -> KnowledgeResult<()> {
    let datasource_id = datasource_id_for(ProviderKind::SqliteAuthority, authority_uri);
    install_provider(ProviderKind::SqliteAuthority, datasource_id, |generation| {
        Ok(Arc::new(ThreadClonedMDB::from_authority_with_scope(
            authority_uri,
            datasource_id_for(ProviderKind::SqliteAuthority, authority_uri),
            generation.0,
        )))
    })
}

pub fn init_mem_provider(memdb: MemDB) -> KnowledgeResult<()> {
    install_provider(
        ProviderKind::SqliteAuthority,
        datasource_id_for(ProviderKind::SqliteAuthority, "memdb"),
        |generation| {
            Ok(Arc::new(MemProvider {
                db: memdb,
                metadata_scope: MetadataCacheScope {
                    datasource_id: datasource_id_for(ProviderKind::SqliteAuthority, "memdb"),
                    generation,
                },
            }))
        },
    )
}

pub fn init_postgres_provider(connection_uri: &str, pool_size: Option<u32>) -> KnowledgeResult<()> {
    let datasource_id = datasource_id_for(ProviderKind::Postgres, connection_uri);
    install_provider(
        ProviderKind::Postgres,
        datasource_id.clone(),
        |generation| {
            let config = PostgresProviderConfig::new(connection_uri).with_pool_size(pool_size);
            let provider = PostgresProvider::connect(
                &config,
                MetadataCacheScope {
                    datasource_id: datasource_id.clone(),
                    generation,
                },
            )?;
            Ok(Arc::new(provider))
        },
    )
}

pub fn init_mysql_provider(connection_uri: &str, pool_size: Option<u32>) -> KnowledgeResult<()> {
    let datasource_id = datasource_id_for(ProviderKind::Mysql, connection_uri);
    install_provider(ProviderKind::Mysql, datasource_id.clone(), |generation| {
        let config = MySqlProviderConfig::new(connection_uri).with_pool_size(pool_size);
        let provider = MySqlProvider::connect(
            &config,
            MetadataCacheScope {
                datasource_id: datasource_id.clone(),
                generation,
            },
        )?;
        Ok(Arc::new(provider))
    })
}

pub fn current_generation() -> Option<u64> {
    runtime()
        .current_generation()
        .map(|generation| generation.0)
}

pub fn runtime_snapshot() -> RuntimeSnapshot {
    runtime().snapshot()
}

pub fn install_runtime_telemetry(
    telemetry_impl: Arc<dyn KnowledgeTelemetry>,
) -> Arc<dyn KnowledgeTelemetry> {
    install_telemetry(telemetry_impl)
}

pub fn query(sql: &str) -> KnowledgeResult<Vec<RowData>> {
    runtime()
        .execute(&QueryRequest::many(sql, Vec::new(), CachePolicy::Bypass))
        .map(QueryResponse::into_rows)
}

pub async fn query_async(sql: &str) -> KnowledgeResult<Vec<RowData>> {
    runtime()
        .execute_async(&QueryRequest::many(sql, Vec::new(), CachePolicy::Bypass))
        .await
        .map(QueryResponse::into_rows)
}

pub fn query_row(sql: &str) -> KnowledgeResult<RowData> {
    runtime()
        .execute(&QueryRequest::first_row(
            sql,
            Vec::new(),
            CachePolicy::Bypass,
        ))
        .map(QueryResponse::into_row)
}

pub async fn query_row_async(sql: &str) -> KnowledgeResult<RowData> {
    runtime()
        .execute_async(&QueryRequest::first_row(
            sql,
            Vec::new(),
            CachePolicy::Bypass,
        ))
        .await
        .map(QueryResponse::into_row)
}

pub fn query_fields(sql: &str, params: &[DataField]) -> KnowledgeResult<RowData> {
    runtime().execute_first_row_fields(sql, params, CachePolicy::Bypass)
}

pub async fn query_fields_async(sql: &str, params: &[DataField]) -> KnowledgeResult<RowData> {
    runtime()
        .execute_first_row_fields_async(sql, params, CachePolicy::Bypass)
        .await
}

pub fn query_named_fields(sql: &str, params: &[DataField]) -> KnowledgeResult<RowData> {
    query_fields(sql, params)
}

pub async fn query_named_fields_async(sql: &str, params: &[DataField]) -> KnowledgeResult<RowData> {
    query_fields_async(sql, params).await
}

pub fn query_named<'a>(
    sql: &str,
    params: &'a [(&'a str, &'a dyn ToSql)],
) -> KnowledgeResult<RowData> {
    let fields = named_params_to_fields(params)?;
    query_fields(sql, &fields)
}

pub fn cache_query_fields<const N: usize>(
    sql: &str,
    c_params: &[DataField; N],
    query_params: &[DataField],
    cache: &mut impl CacheAble<DataField, RowData, N>,
) -> RowData {
    cache_query_fields_with_scope(sql, stable_hash(sql), c_params, query_params, cache)
}

pub fn cache_query_fields_with_scope<const N: usize>(
    sql: &str,
    local_cache_scope: u64,
    c_params: &[DataField; N],
    query_params: &[DataField],
    cache: &mut impl CacheAble<DataField, RowData, N>,
) -> RowData {
    if let Some(generation) = current_generation() {
        cache.prepare_generation(generation);
    }
    if let Some(hit) = cache.fetch_scoped(local_cache_scope, c_params) {
        runtime().record_local_cache_hit();
        if telemetry_enabled() {
            telemetry().on_cache(&CacheTelemetryEvent {
                layer: CacheLayer::Local,
                outcome: CacheOutcome::Hit,
                provider_kind: runtime().current_provider_kind(),
            });
        }
        return hit.clone();
    }
    runtime().record_local_cache_miss();
    if telemetry_enabled() {
        telemetry().on_cache(&CacheTelemetryEvent {
            layer: CacheLayer::Local,
            outcome: CacheOutcome::Miss,
            provider_kind: runtime().current_provider_kind(),
        });
    }

    match runtime().execute_first_row_fields(sql, query_params, CachePolicy::UseGlobal) {
        Ok(row) => {
            cache.save_scoped(local_cache_scope, c_params, row.clone());
            row
        }
        Err(err) => {
            warn_kdb!("[kdb] query error: {}", err);
            Vec::new()
        }
    }
}

pub async fn cache_query_fields_async<const N: usize>(
    sql: &str,
    c_params: &[DataField; N],
    query_params: &[DataField],
    cache: &mut impl CacheAble<DataField, RowData, N>,
) -> RowData {
    cache_query_fields_async_with_scope(
        sql,
        stable_hash(sql),
        c_params,
        || query_params.to_vec(),
        cache,
    )
    .await
}

pub async fn cache_query_fields_async_with<const N: usize, F>(
    sql: &str,
    c_params: &[DataField; N],
    build_query_params: F,
    cache: &mut impl CacheAble<DataField, RowData, N>,
) -> RowData
where
    F: FnOnce() -> Vec<DataField>,
{
    cache_query_fields_async_with_scope(sql, stable_hash(sql), c_params, build_query_params, cache)
        .await
}

pub async fn cache_query_fields_async_with_scope<const N: usize, F>(
    sql: &str,
    local_cache_scope: u64,
    c_params: &[DataField; N],
    build_query_params: F,
    cache: &mut impl CacheAble<DataField, RowData, N>,
) -> RowData
where
    F: FnOnce() -> Vec<DataField>,
{
    if let Some(generation) = current_generation() {
        cache.prepare_generation(generation);
    }
    if let Some(hit) = cache.fetch_scoped(local_cache_scope, c_params) {
        runtime().record_local_cache_hit();
        if telemetry_enabled() {
            telemetry().on_cache(&CacheTelemetryEvent {
                layer: CacheLayer::Local,
                outcome: CacheOutcome::Hit,
                provider_kind: runtime().current_provider_kind(),
            });
        }
        return hit.clone();
    }
    runtime().record_local_cache_miss();
    if telemetry_enabled() {
        telemetry().on_cache(&CacheTelemetryEvent {
            layer: CacheLayer::Local,
            outcome: CacheOutcome::Miss,
            provider_kind: runtime().current_provider_kind(),
        });
    }

    let query_params = build_query_params();
    match runtime()
        .execute_first_row_fields_async(sql, &query_params, CachePolicy::UseGlobal)
        .await
    {
        Ok(row) => {
            cache.save_scoped(local_cache_scope, c_params, row.clone());
            row
        }
        Err(err) => {
            warn_kdb!("[kdb] query error: {}", err);
            Vec::new()
        }
    }
}

pub fn cache_query<const N: usize>(
    sql: &str,
    c_params: &[DataField; N],
    named_params: &[(&str, &dyn ToSql)],
    cache: &mut impl CacheAble<DataField, RowData, N>,
) -> RowData {
    let query_fields = match named_params_to_fields(named_params) {
        Ok(fields) => fields,
        Err(err) => {
            warn_kdb!("[kdb] query param conversion error: {}", err);
            return Vec::new();
        }
    };

    cache_query_fields(sql, c_params, &query_fields, cache)
}

pub async fn cache_query_async<const N: usize>(
    sql: &str,
    c_params: &[DataField; N],
    named_params: &[(&str, &dyn ToSql)],
    cache: &mut impl CacheAble<DataField, RowData, N>,
) -> RowData {
    let query_fields = match named_params_to_fields(named_params) {
        Ok(fields) => fields,
        Err(err) => {
            warn_kdb!("[kdb] query param conversion error: {}", err);
            return Vec::new();
        }
    };

    cache_query_fields_async(sql, c_params, &query_fields, cache).await
}

fn ensure_wal(authority_uri: &str) -> KnowledgeResult<()> {
    if let Ok(conn) = Connection::open_with_flags(
        authority_uri,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI,
    ) {
        let _ = conn.execute_batch(
            "PRAGMA journal_mode=WAL;\nPRAGMA synchronous=NORMAL;\nPRAGMA temp_store=MEMORY;",
        );
    }
    Ok(())
}

pub fn init_wal_pool_from_authority(authority_uri: &str, pool_size: u32) -> KnowledgeResult<()> {
    ensure_wal(authority_uri)?;
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let mem = MemDB::new_file(authority_uri, pool_size, flags)?;
    init_mem_provider(mem)
}

pub fn init_thread_cloned_from_knowdb(
    root: &Path,
    knowdb_conf: &Path,
    authority_uri: &str,
    dict: &orion_variate::EnvDict,
) -> KnowledgeResult<()> {
    let (conf, conf_abs, _) = parse_knowdb_conf(root, knowdb_conf, dict)?;
    if let Some(provider) = conf.provider {
        match provider.kind {
            ProviderKind::Postgres => {
                info_ctrl!("init postgres knowdb provider({}) ", conf_abs.display(),);
                init_postgres_provider(provider.connection_uri.as_str(), provider.pool_size)?;
                runtime().configure_result_cache(
                    conf.cache.enabled,
                    conf.cache.capacity,
                    Duration::from_millis(conf.cache.ttl_ms.max(1)),
                );
                return Ok(());
            }
            ProviderKind::Mysql => {
                info_ctrl!("init mysql knowdb provider({}) ", conf_abs.display(),);
                init_mysql_provider(provider.connection_uri.as_str(), provider.pool_size)?;
                runtime().configure_result_cache(
                    conf.cache.enabled,
                    conf.cache.capacity,
                    Duration::from_millis(conf.cache.ttl_ms.max(1)),
                );
                return Ok(());
            }
            ProviderKind::SqliteAuthority => {}
        }
    }

    crate::loader::build_authority_from_knowdb(root, knowdb_conf, authority_uri, dict)?;
    let ro_uri = if let Some(rest) = authority_uri.strip_prefix("file:") {
        let path_part = rest.split('?').next().unwrap_or(rest);
        format!("file:{}?mode=ro&uri=true", path_part)
    } else {
        authority_uri.to_string()
    };

    info_ctrl!("init authority knowdb success({}) ", knowdb_conf.display(),);
    init_thread_cloned_from_authority(&ro_uri)?;
    runtime().configure_result_cache(
        conf.cache.enabled,
        conf.cache.capacity,
        Duration::from_millis(conf.cache.ttl_ms.max(1)),
    );
    Ok(())
}

fn stable_hash(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::FieldQueryCache;
    use crate::mem::memdb::MemDB;
    use crate::mem::query_util::{COLNAME_CACHE, metadata_cache_key_for_scope};
    use crate::runtime::fields_to_params;
    use crate::telemetry::{
        CacheLayer, CacheTelemetryEvent, KnowledgeTelemetry, QueryTelemetryEvent,
        ReloadTelemetryEvent, reset_telemetry,
    };
    use orion_error::{ToStructError, UvsFrom};
    use orion_variate::EnvDict;
    use std::fs;
    use std::hint::black_box;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};
    use wp_error::KnowledgeReason;

    #[derive(Default)]
    struct TestTelemetry {
        reload_success: AtomicU64,
        reload_failure: AtomicU64,
        local_hits: AtomicU64,
        local_misses: AtomicU64,
        result_hits: AtomicU64,
        result_misses: AtomicU64,
        metadata_hits: AtomicU64,
        metadata_misses: AtomicU64,
        query_success: AtomicU64,
        query_failure: AtomicU64,
    }

    impl KnowledgeTelemetry for TestTelemetry {
        fn on_cache(&self, event: &CacheTelemetryEvent) {
            let counter = match (event.layer, event.outcome) {
                (CacheLayer::Local, CacheOutcome::Hit) => &self.local_hits,
                (CacheLayer::Local, CacheOutcome::Miss) => &self.local_misses,
                (CacheLayer::Result, CacheOutcome::Hit) => &self.result_hits,
                (CacheLayer::Result, CacheOutcome::Miss) => &self.result_misses,
                (CacheLayer::Metadata, CacheOutcome::Hit) => &self.metadata_hits,
                (CacheLayer::Metadata, CacheOutcome::Miss) => &self.metadata_misses,
            };
            counter.fetch_add(1, Ordering::Relaxed);
        }

        fn on_reload(&self, event: &ReloadTelemetryEvent) {
            let counter = match event.outcome {
                crate::telemetry::ReloadOutcome::Success => &self.reload_success,
                crate::telemetry::ReloadOutcome::Failure => &self.reload_failure,
            };
            counter.fetch_add(1, Ordering::Relaxed);
        }

        fn on_query(&self, event: &QueryTelemetryEvent) {
            let counter = if event.success {
                &self.query_success
            } else {
                &self.query_failure
            };
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn perf_env_usize(key: &str, default: usize) -> usize {
        std::env::var(key)
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(default)
    }

    fn seed_perf_provider(rows: usize) {
        let db = MemDB::instance();
        db.execute("CREATE TABLE perf_kv (id INTEGER PRIMARY KEY, value TEXT)")
            .expect("create perf_kv");
        db.execute("BEGIN IMMEDIATE").expect("begin perf_kv load");
        for id in 1..=rows {
            let sql = format!("INSERT INTO perf_kv (id, value) VALUES ({id}, 'value_{id}')");
            db.execute(sql.as_str()).expect("insert perf_kv row");
        }
        db.execute("COMMIT").expect("commit perf_kv load");
        init_mem_provider(db).expect("init perf provider");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn query_async_works_with_mem_provider() {
        let _guard = crate::runtime::runtime_test_guard().lock_async().await;
        let db = MemDB::instance();
        db.execute("CREATE TABLE async_kv (id INTEGER PRIMARY KEY, value TEXT)")
            .expect("create async_kv");
        db.execute("INSERT INTO async_kv (id, value) VALUES (1, 'hello')")
            .expect("insert async_kv row");
        init_mem_provider(db).expect("init mem provider");

        let row = query_fields_async(
            "SELECT value FROM async_kv WHERE id=:id",
            &[DataField::from_digit(":id", 1)],
        )
        .await
        .expect("query async row");
        assert_eq!(row.len(), 1);
        assert_eq!(row[0].to_string(), "chars(hello)");
    }

    #[derive(Clone)]
    struct PerfQuery {
        cache_key: [DataField; 1],
        query_params: [DataField; 1],
        bypass_req: QueryRequest,
        global_req: QueryRequest,
    }

    fn build_perf_workload(ops: usize, hotset: usize) -> Vec<PerfQuery> {
        (0..ops)
            .map(|idx| {
                let id = ((idx * 17) % hotset + 1) as i64;
                let cache_key = [DataField::from_digit("id", id)];
                let query_params = [DataField::from_digit(":id", id)];
                let bypass_req = QueryRequest::first_row(
                    "SELECT value FROM perf_kv WHERE id=:id",
                    fields_to_params(&query_params),
                    CachePolicy::Bypass,
                );
                let global_req = QueryRequest::first_row(
                    "SELECT value FROM perf_kv WHERE id=:id",
                    fields_to_params(&query_params),
                    CachePolicy::UseGlobal,
                );
                PerfQuery {
                    cache_key,
                    query_params,
                    bypass_req,
                    global_req,
                }
            })
            .collect()
    }

    #[derive(Debug, Clone, Copy)]
    struct PerfCounters {
        result_hits: u64,
        result_misses: u64,
        local_hits: u64,
        local_misses: u64,
        metadata_hits: u64,
        metadata_misses: u64,
    }

    #[derive(Debug, Clone)]
    struct PerfResult {
        name: &'static str,
        elapsed: Duration,
        ops: usize,
        counters: PerfCounters,
    }

    impl PerfResult {
        fn qps(&self) -> f64 {
            let secs = self.elapsed.as_secs_f64();
            if secs == 0.0 {
                self.ops as f64
            } else {
                self.ops as f64 / secs
            }
        }
    }

    fn snapshot_delta(before: &RuntimeSnapshot, after: &RuntimeSnapshot) -> PerfCounters {
        PerfCounters {
            result_hits: after
                .result_cache_hits
                .saturating_sub(before.result_cache_hits),
            result_misses: after
                .result_cache_misses
                .saturating_sub(before.result_cache_misses),
            local_hits: after
                .local_cache_hits
                .saturating_sub(before.local_cache_hits),
            local_misses: after
                .local_cache_misses
                .saturating_sub(before.local_cache_misses),
            metadata_hits: after
                .metadata_cache_hits
                .saturating_sub(before.metadata_cache_hits),
            metadata_misses: after
                .metadata_cache_misses
                .saturating_sub(before.metadata_cache_misses),
        }
    }

    fn run_bypass_perf(workload: &[PerfQuery], rows: usize) -> PerfResult {
        seed_perf_provider(rows);
        let before = runtime_snapshot();
        let started = Instant::now();
        for item in workload {
            let row = runtime()
                .execute(&item.bypass_req)
                .expect("execute bypass request")
                .into_row();
            black_box(row);
        }
        let elapsed = started.elapsed();
        let after = runtime_snapshot();
        PerfResult {
            name: "bypass",
            elapsed,
            ops: workload.len(),
            counters: snapshot_delta(&before, &after),
        }
    }

    fn run_global_cache_perf(workload: &[PerfQuery], rows: usize) -> PerfResult {
        seed_perf_provider(rows);
        let before = runtime_snapshot();
        let started = Instant::now();
        for item in workload {
            let row = runtime()
                .execute(&item.global_req)
                .expect("execute global-cache request")
                .into_row();
            black_box(row);
        }
        let elapsed = started.elapsed();
        let after = runtime_snapshot();
        PerfResult {
            name: "global_cache",
            elapsed,
            ops: workload.len(),
            counters: snapshot_delta(&before, &after),
        }
    }

    fn run_local_cache_perf(workload: &[PerfQuery], rows: usize) -> PerfResult {
        seed_perf_provider(rows);
        let mut cache = FieldQueryCache::with_capacity(workload.len().max(1));
        let before = runtime_snapshot();
        let started = Instant::now();
        for item in workload {
            let row = cache_query_fields(
                "SELECT value FROM perf_kv WHERE id=:id",
                &item.cache_key,
                &item.query_params,
                &mut cache,
            );
            black_box(row);
        }
        let elapsed = started.elapsed();
        let after = runtime_snapshot();
        PerfResult {
            name: "local_cache",
            elapsed,
            ops: workload.len(),
            counters: snapshot_delta(&before, &after),
        }
    }

    fn print_perf_result(result: &PerfResult) {
        eprintln!(
            "[wp-knowledge][cache-perf] scenario={} elapsed_ms={} qps={:.0} result_hit={} result_miss={} local_hit={} local_miss={} metadata_hit={} metadata_miss={}",
            result.name,
            result.elapsed.as_millis(),
            result.qps(),
            result.counters.result_hits,
            result.counters.result_misses,
            result.counters.local_hits,
            result.counters.local_misses,
            result.counters.metadata_hits,
            result.counters.metadata_misses,
        );
    }

    fn uniq_cache_cfg_tmp_dir() -> PathBuf {
        use rand::{Rng, rng};
        let rnd: u64 = rng().next_u64();
        std::env::temp_dir().join(format!("wpk_cache_cfg_{}", rnd))
    }

    fn write_minimal_knowdb_with_cache(
        root: &Path,
        enabled: bool,
        capacity: usize,
        ttl_ms: u64,
    ) -> std::path::PathBuf {
        let models = root.join("models").join("knowledge");
        let example_dir = models.join("example");
        fs::create_dir_all(&example_dir).expect("create knowdb models/example");
        fs::write(
            models.join("knowdb.toml"),
            format!(
                r#"
version = 2
base_dir = "."

[cache]
enabled = {enabled}
capacity = {capacity}
ttl_ms = {ttl_ms}

[csv]
has_header = false

[[tables]]
name = "example"
columns.by_index = [0,1]
"#
            ),
        )
        .expect("write knowdb.toml");
        fs::write(
            example_dir.join("create.sql"),
            r#"
CREATE TABLE IF NOT EXISTS {table} (
  id INTEGER PRIMARY KEY,
  value TEXT NOT NULL
);
"#,
        )
        .expect("write create.sql");
        fs::write(
            example_dir.join("insert.sql"),
            "INSERT INTO {table} (id, value) VALUES (?1, ?2);\n",
        )
        .expect("write insert.sql");
        fs::write(example_dir.join("data.csv"), "1,alpha\n").expect("write data.csv");
        models.join("knowdb.toml")
    }

    fn write_provider_only_knowdb_with_cache(
        root: &Path,
        provider_kind: &str,
        connection_uri: &str,
        enabled: bool,
        capacity: usize,
        ttl_ms: u64,
    ) -> std::path::PathBuf {
        let models = root.join("models").join("knowledge");
        fs::create_dir_all(&models).expect("create knowdb models");
        fs::write(
            models.join("knowdb.toml"),
            format!(
                r#"
version = 2
base_dir = "."

[cache]
enabled = {enabled}
capacity = {capacity}
ttl_ms = {ttl_ms}

[provider]
kind = "{provider_kind}"
connection_uri = "{connection_uri}"
"#
            ),
        )
        .expect("write provider knowdb.toml");
        models.join("knowdb.toml")
    }

    fn restore_default_result_cache_config() {
        runtime().configure_result_cache(true, 1024, Duration::from_millis(30_000));
    }

    #[test]
    fn provider_can_be_replaced() {
        let _guard = crate::runtime::runtime_test_guard()
            .lock()
            .expect("provider test guard");
        let db1 = MemDB::instance();
        db1.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, value TEXT)")
            .expect("create table in db1");
        db1.execute("INSERT INTO t (id, value) VALUES (1, 'first')")
            .expect("seed db1");
        init_mem_provider(db1).expect("init provider db1");
        let row = query_row("SELECT value FROM t WHERE id = 1").expect("query db1");
        assert_eq!(row[0].to_string(), "chars(first)");

        let db2 = MemDB::instance();
        db2.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, value TEXT)")
            .expect("create table in db2");
        db2.execute("INSERT INTO t (id, value) VALUES (1, 'second')")
            .expect("seed db2");
        init_mem_provider(db2).expect("replace provider with db2");
        let row = query_row("SELECT value FROM t WHERE id = 1").expect("query db2");
        assert_eq!(row[0].to_string(), "chars(second)");
    }

    #[test]
    fn sqlite_metadata_cache_uses_provider_scope_after_reload() {
        let _guard = crate::runtime::runtime_test_guard()
            .lock()
            .expect("provider test guard");
        COLNAME_CACHE.write().expect("metadata cache lock").clear();

        let db = MemDB::instance();
        db.execute("CREATE TABLE cache_scope_t (id INTEGER PRIMARY KEY, value TEXT)")
            .expect("create table");
        db.execute("INSERT INTO cache_scope_t (id, value) VALUES (1, 'scope-old')")
            .expect("seed table");

        let old_scope = MetadataCacheScope {
            datasource_id: DatasourceId("sqlite:old".to_string()),
            generation: Generation(1),
        };
        let new_scope = MetadataCacheScope {
            datasource_id: DatasourceId("sqlite:new".to_string()),
            generation: Generation(2),
        };
        let old_provider = MemProvider {
            db: db.clone(),
            metadata_scope: old_scope.clone(),
        };

        install_provider(
            ProviderKind::SqliteAuthority,
            new_scope.datasource_id.clone(),
            |_generation| {
                Ok(Arc::new(MemProvider {
                    db: db.clone(),
                    metadata_scope: new_scope.clone(),
                }))
            },
        )
        .expect("install new provider");

        let row = old_provider
            .query_row("SELECT value FROM cache_scope_t WHERE id = 1")
            .expect("old provider query");
        assert_eq!(row[0].to_string(), "chars(scope-old)");

        let cache = COLNAME_CACHE.read().expect("metadata cache lock");
        assert!(cache.contains(&metadata_cache_key_for_scope(
            &old_scope,
            "SELECT value FROM cache_scope_t WHERE id = 1",
        )));
        assert!(!cache.contains(&metadata_cache_key_for_scope(
            &new_scope,
            "SELECT value FROM cache_scope_t WHERE id = 1",
        )));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_query_uses_runtime_bridge() {
        let _guard = crate::runtime::runtime_test_guard().lock_async().await;
        let db = MemDB::instance();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, value TEXT)")
            .expect("create table");
        db.execute("INSERT INTO t (id, value) VALUES (1, 'async-first')")
            .expect("seed table");
        init_mem_provider(db).expect("init provider");

        let row = query_row_async("SELECT value FROM t WHERE id = 1")
            .await
            .expect("async query row");
        assert_eq!(row[0].to_string(), "chars(async-first)");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_cache_query_fields_hits_local_cache() {
        let _guard = crate::runtime::runtime_test_guard().lock_async().await;
        let db = MemDB::instance();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, value TEXT)")
            .expect("create table");
        db.execute("INSERT INTO t (id, value) VALUES (1, 'async-cache')")
            .expect("seed table");
        init_mem_provider(db).expect("init provider");

        let key = [DataField::from_digit("id", 1)];
        let params = [DataField::from_digit(":id", 1)];
        let mut cache = FieldQueryCache::default();

        let first = cache_query_fields_async(
            "SELECT value FROM t WHERE id=:id",
            &key,
            &params,
            &mut cache,
        )
        .await;
        let second = cache_query_fields_async(
            "SELECT value FROM t WHERE id=:id",
            &key,
            &params,
            &mut cache,
        )
        .await;

        assert_eq!(first[0].to_string(), "chars(async-cache)");
        assert_eq!(second[0].to_string(), "chars(async-cache)");
    }

    #[test]
    fn local_cache_is_cleared_when_generation_changes() {
        let _guard = crate::runtime::runtime_test_guard()
            .lock()
            .expect("provider test guard");
        let db1 = MemDB::instance();
        db1.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, value TEXT)")
            .expect("create table in db1");
        db1.execute("INSERT INTO t (id, value) VALUES (1, 'first')")
            .expect("seed db1");
        init_mem_provider(db1).expect("init provider db1");

        let key = [DataField::from_digit("id", 1)];
        let params = [DataField::from_digit(":id", 1)];
        let mut cache = FieldQueryCache::default();
        let row = cache_query_fields(
            "SELECT value FROM t WHERE id=:id",
            &key,
            &params,
            &mut cache,
        );
        assert_eq!(row[0].to_string(), "chars(first)");

        let db2 = MemDB::instance();
        db2.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, value TEXT)")
            .expect("create table in db2");
        db2.execute("INSERT INTO t (id, value) VALUES (1, 'second')")
            .expect("seed db2");
        init_mem_provider(db2).expect("replace provider with db2");

        let row = cache_query_fields(
            "SELECT value FROM t WHERE id=:id",
            &key,
            &params,
            &mut cache,
        );
        assert_eq!(row[0].to_string(), "chars(second)");
    }

    #[test]
    fn local_cache_is_scoped_by_sql_text() {
        let _guard = crate::runtime::runtime_test_guard()
            .lock()
            .expect("provider test guard");
        let db = MemDB::instance();
        db.execute("CREATE TABLE t1 (id INTEGER PRIMARY KEY, value TEXT)")
            .expect("create t1");
        db.execute("CREATE TABLE t2 (id INTEGER PRIMARY KEY, value TEXT)")
            .expect("create t2");
        db.execute("INSERT INTO t1 (id, value) VALUES (1, 'first')")
            .expect("seed t1");
        db.execute("INSERT INTO t2 (id, value) VALUES (1, 'second')")
            .expect("seed t2");
        init_mem_provider(db).expect("init provider");

        let key = [DataField::from_digit("id", 1)];
        let params = [DataField::from_digit(":id", 1)];
        let mut cache = FieldQueryCache::default();

        let row = cache_query_fields(
            "SELECT value FROM t1 WHERE id=:id",
            &key,
            &params,
            &mut cache,
        );
        assert_eq!(row[0].to_string(), "chars(first)");

        let row = cache_query_fields(
            "SELECT value FROM t2 WHERE id=:id",
            &key,
            &params,
            &mut cache,
        );
        assert_eq!(row[0].to_string(), "chars(second)");
    }

    #[test]
    fn runtime_snapshot_tracks_generation_and_cache_size() {
        let _guard = crate::runtime::runtime_test_guard()
            .lock()
            .expect("provider test guard");
        let db = MemDB::instance();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, value TEXT)")
            .expect("create table");
        db.execute("INSERT INTO t (id, value) VALUES (1, 'first')")
            .expect("seed table");
        init_mem_provider(db).expect("init provider");

        let mut cache = FieldQueryCache::default();
        let key = [DataField::from_digit("id", 1)];
        let params = [DataField::from_digit(":id", 1)];
        let row = cache_query_fields(
            "SELECT value FROM t WHERE id=:id",
            &key,
            &params,
            &mut cache,
        );
        assert_eq!(row[0].to_string(), "chars(first)");

        let snapshot = runtime_snapshot();
        assert!(matches!(
            snapshot.provider_kind,
            Some(ProviderKind::SqliteAuthority)
        ));
        assert!(snapshot.generation.is_some());
        assert!(snapshot.result_cache_len >= 1);
        assert!(snapshot.result_cache_capacity >= snapshot.result_cache_len);
        assert!(snapshot.metadata_cache_capacity >= snapshot.metadata_cache_len);
        assert!(snapshot.reload_successes >= 1);
    }

    #[test]
    fn metadata_cache_is_scoped_by_generation() {
        let _guard = crate::runtime::runtime_test_guard()
            .lock()
            .expect("provider test guard");
        let sql = "SELECT value FROM t WHERE id = 1";
        let before = runtime_snapshot().metadata_cache_len;

        let db1 = MemDB::instance();
        db1.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, value TEXT)")
            .expect("create table in db1");
        db1.execute("INSERT INTO t (id, value) VALUES (1, 'first')")
            .expect("seed db1");
        init_mem_provider(db1).expect("init provider db1");
        let row = query_row(sql).expect("query db1");
        assert_eq!(row[0].to_string(), "chars(first)");
        let after_first = runtime_snapshot().metadata_cache_len;
        assert!(
            after_first > before,
            "metadata cache did not record first generation entry: before={before} after_first={after_first}"
        );

        let db2 = MemDB::instance();
        db2.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, value TEXT)")
            .expect("create table in db2");
        db2.execute("INSERT INTO t (id, value) VALUES (1, 'second')")
            .expect("seed db2");
        init_mem_provider(db2).expect("replace provider with db2");
        let row = query_row(sql).expect("query db2");
        assert_eq!(row[0].to_string(), "chars(second)");
        let after_second = runtime_snapshot().metadata_cache_len;
        assert!(
            after_second > after_first,
            "metadata cache did not keep a distinct generation entry: after_first={after_first} after_second={after_second}"
        );
    }

    #[test]
    fn failed_provider_reload_keeps_previous_provider() {
        let _guard = crate::runtime::runtime_test_guard()
            .lock()
            .expect("provider test guard");
        let db1 = MemDB::instance();
        db1.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, value TEXT)")
            .expect("create table in db1");
        db1.execute("INSERT INTO t (id, value) VALUES (1, 'first')")
            .expect("seed db1");
        init_mem_provider(db1).expect("init provider db1");
        let before_generation = current_generation();

        let reload_err = install_provider(
            ProviderKind::SqliteAuthority,
            datasource_id_for(ProviderKind::SqliteAuthority, "reload-failure"),
            |_generation| {
                Err(KnowledgeReason::from_logic()
                    .to_err()
                    .with_detail("expected reload failure"))
            },
        );
        assert!(reload_err.is_err());

        let row = query_row("SELECT value FROM t WHERE id = 1").expect("query previous provider");
        assert_eq!(row[0].to_string(), "chars(first)");
        assert_eq!(current_generation(), before_generation);
    }

    #[test]
    fn runtime_snapshot_records_cache_counters() {
        let _guard = crate::runtime::runtime_test_guard()
            .lock()
            .expect("provider test guard");
        let db = MemDB::instance();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, value TEXT)")
            .expect("create table");
        db.execute("INSERT INTO t (id, value) VALUES (1, 'first')")
            .expect("seed table");
        init_mem_provider(db).expect("init provider");

        let before = runtime_snapshot();
        let mut cache = FieldQueryCache::default();
        let key = [DataField::from_digit("id", 1)];
        let params = [DataField::from_digit(":id", 1)];
        let row = cache_query_fields(
            "SELECT value FROM t WHERE id=:id",
            &key,
            &params,
            &mut cache,
        );
        assert_eq!(row[0].to_string(), "chars(first)");
        let row = cache_query_fields(
            "SELECT value FROM t WHERE id=:id",
            &key,
            &params,
            &mut cache,
        );
        assert_eq!(row[0].to_string(), "chars(first)");

        let after = runtime_snapshot();
        assert!(after.local_cache_hits > before.local_cache_hits);
        assert!(after.local_cache_misses > before.local_cache_misses);
        assert!(after.result_cache_misses > before.result_cache_misses);
        assert!(after.metadata_cache_misses > before.metadata_cache_misses);
    }

    #[test]
    fn telemetry_receives_reload_cache_and_query_events() {
        let _guard = crate::runtime::runtime_test_guard()
            .lock()
            .expect("provider test guard");
        let telemetry_impl = Arc::new(TestTelemetry::default());
        let previous = install_runtime_telemetry(telemetry_impl.clone());

        let db = MemDB::instance();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, value TEXT)")
            .expect("create table");
        db.execute("INSERT INTO t (id, value) VALUES (1, 'first')")
            .expect("seed table");
        init_mem_provider(db).expect("init provider");

        let mut cache = FieldQueryCache::default();
        let key = [DataField::from_digit("id", 1)];
        let params = [DataField::from_digit(":id", 1)];
        let row = cache_query_fields(
            "SELECT value FROM t WHERE id=:id",
            &key,
            &params,
            &mut cache,
        );
        assert_eq!(row[0].to_string(), "chars(first)");
        let row = cache_query_fields(
            "SELECT value FROM t WHERE id=:id",
            &key,
            &params,
            &mut cache,
        );
        assert_eq!(row[0].to_string(), "chars(first)");

        let reload_err = install_provider(
            ProviderKind::SqliteAuthority,
            datasource_id_for(ProviderKind::SqliteAuthority, "telemetry-failure"),
            |_generation| {
                Err(KnowledgeReason::from_logic()
                    .to_err()
                    .with_detail("expected telemetry reload failure"))
            },
        );
        assert!(reload_err.is_err());

        install_runtime_telemetry(previous);
        reset_telemetry();

        assert!(telemetry_impl.reload_success.load(Ordering::Relaxed) >= 1);
        assert!(telemetry_impl.reload_failure.load(Ordering::Relaxed) >= 1);
        assert!(telemetry_impl.local_hits.load(Ordering::Relaxed) >= 1);
        assert!(telemetry_impl.local_misses.load(Ordering::Relaxed) >= 1);
        assert!(telemetry_impl.result_misses.load(Ordering::Relaxed) >= 1);
        assert!(telemetry_impl.metadata_misses.load(Ordering::Relaxed) >= 1);
        assert!(telemetry_impl.query_success.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    #[ignore = "manual perf comparison; run with cargo test cache_perf_reports_cache_vs_no_cache -- --ignored --nocapture"]
    fn cache_perf_reports_cache_vs_no_cache() {
        let _guard = crate::runtime::runtime_test_guard()
            .lock()
            .expect("provider test guard");
        let rows = perf_env_usize("WP_KDB_PERF_ROWS", 10_000).max(1);
        let ops = perf_env_usize("WP_KDB_PERF_OPS", 120_000).max(1);
        let hotset = perf_env_usize("WP_KDB_PERF_HOTSET", 128).clamp(1, rows);
        let workload = build_perf_workload(ops, hotset);

        eprintln!(
            "[wp-knowledge][cache-perf] rows={} ops={} hotset={} sql=SELECT value FROM perf_kv WHERE id=:id",
            rows, ops, hotset
        );

        let bypass = run_bypass_perf(&workload, rows);
        let global = run_global_cache_perf(&workload, rows);
        let local = run_local_cache_perf(&workload, rows);

        print_perf_result(&bypass);
        print_perf_result(&global);
        print_perf_result(&local);

        eprintln!(
            "[wp-knowledge][cache-perf] speedup global_vs_bypass={:.2}x local_vs_bypass={:.2}x",
            bypass.elapsed.as_secs_f64() / global.elapsed.as_secs_f64(),
            bypass.elapsed.as_secs_f64() / local.elapsed.as_secs_f64(),
        );

        assert_eq!(bypass.counters.result_hits, 0);
        assert_eq!(bypass.counters.result_misses, 0);
        assert_eq!(bypass.counters.local_hits, 0);
        assert_eq!(bypass.counters.local_misses, 0);
        assert!(global.counters.result_hits > 0);
        assert!(global.counters.result_misses > 0);
        assert_eq!(global.counters.local_hits, 0);
        assert_eq!(global.counters.local_misses, 0);
        assert!(local.counters.local_hits > 0);
        assert!(local.counters.local_misses > 0);
        assert!(local.counters.result_misses > 0);
    }

    #[test]
    fn init_thread_cloned_from_knowdb_applies_result_cache_config() {
        let _guard = crate::runtime::runtime_test_guard()
            .lock()
            .expect("provider test guard");
        let root = uniq_cache_cfg_tmp_dir();
        let conf_path = write_minimal_knowdb_with_cache(&root, true, 7, 5);
        let auth_file = root.join(".run").join("authority.sqlite");
        fs::create_dir_all(auth_file.parent().expect("authority parent"))
            .expect("create authority parent");
        let authority_uri = format!("file:{}?mode=rwc&uri=true", auth_file.display());

        init_thread_cloned_from_knowdb(&root, &conf_path, &authority_uri, &EnvDict::default())
            .expect("init knowdb with cache config");

        let snapshot = runtime_snapshot();
        assert!(snapshot.result_cache_enabled);
        assert_eq!(snapshot.result_cache_capacity, 7);
        assert_eq!(snapshot.result_cache_ttl_ms, 5);

        let req = QueryRequest::first_row(
            "SELECT value FROM example WHERE id=:id",
            fields_to_params(&[DataField::from_digit(":id", 1)]),
            CachePolicy::UseGlobal,
        );
        let before = runtime_snapshot();
        let row = runtime()
            .execute(&req)
            .expect("first result-cache query")
            .into_row();
        assert_eq!(row[0].to_string(), "chars(alpha)");
        let row = runtime()
            .execute(&req)
            .expect("second result-cache query")
            .into_row();
        assert_eq!(row[0].to_string(), "chars(alpha)");
        std::thread::sleep(Duration::from_millis(12));
        let row = runtime()
            .execute(&req)
            .expect("expired result-cache query")
            .into_row();
        assert_eq!(row[0].to_string(), "chars(alpha)");
        let after = runtime_snapshot();

        assert!(after.result_cache_hits > before.result_cache_hits);
        assert!(after.result_cache_misses >= before.result_cache_misses + 2);

        restore_default_result_cache_config();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn disabled_result_cache_from_knowdb_config_forces_bypass() {
        let _guard = crate::runtime::runtime_test_guard()
            .lock()
            .expect("provider test guard");
        let root = uniq_cache_cfg_tmp_dir();
        let conf_path = write_minimal_knowdb_with_cache(&root, false, 3, 30_000);
        let auth_file = root.join(".run").join("authority.sqlite");
        fs::create_dir_all(auth_file.parent().expect("authority parent"))
            .expect("create authority parent");
        let authority_uri = format!("file:{}?mode=rwc&uri=true", auth_file.display());

        init_thread_cloned_from_knowdb(&root, &conf_path, &authority_uri, &EnvDict::default())
            .expect("init knowdb with cache disabled");

        let snapshot = runtime_snapshot();
        assert!(!snapshot.result_cache_enabled);
        assert_eq!(snapshot.result_cache_capacity, 3);
        assert_eq!(snapshot.result_cache_ttl_ms, 30_000);

        let req = QueryRequest::first_row(
            "SELECT value FROM example WHERE id=:id",
            fields_to_params(&[DataField::from_digit(":id", 1)]),
            CachePolicy::UseGlobal,
        );
        let before = runtime_snapshot();
        let _ = runtime()
            .execute(&req)
            .expect("first bypassed result-cache query");
        let _ = runtime()
            .execute(&req)
            .expect("second bypassed result-cache query");
        let after = runtime_snapshot();

        assert_eq!(after.result_cache_hits, before.result_cache_hits);
        assert_eq!(after.result_cache_misses, before.result_cache_misses);

        restore_default_result_cache_config();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn failed_knowdb_provider_init_does_not_apply_cache_config() {
        let _guard = crate::runtime::runtime_test_guard()
            .lock()
            .expect("provider test guard");
        restore_default_result_cache_config();

        let db = MemDB::instance();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, value TEXT)")
            .expect("create table");
        db.execute("INSERT INTO t (id, value) VALUES (1, 'first')")
            .expect("seed table");
        init_mem_provider(db).expect("init provider");

        let before = runtime_snapshot();
        let root = uniq_cache_cfg_tmp_dir();
        let conf_path = write_provider_only_knowdb_with_cache(
            &root,
            "mysql",
            "not-a-valid-mysql-url",
            false,
            3,
            5,
        );
        let authority_uri = format!(
            "file:{}?mode=rwc&uri=true",
            root.join("unused.sqlite").display()
        );

        let err =
            init_thread_cloned_from_knowdb(&root, &conf_path, &authority_uri, &EnvDict::default());
        assert!(err.is_err());

        let after = runtime_snapshot();
        assert_eq!(after.result_cache_enabled, before.result_cache_enabled);
        assert_eq!(after.result_cache_capacity, before.result_cache_capacity);
        assert_eq!(after.result_cache_ttl_ms, before.result_cache_ttl_ms);

        let row = query_row("SELECT value FROM t WHERE id = 1").expect("query previous provider");
        assert_eq!(row[0].to_string(), "chars(first)");

        let _ = fs::remove_dir_all(&root);
    }
}
