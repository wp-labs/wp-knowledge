use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use tokio::sync::Barrier;
use tokio::task::JoinSet;
use tokio_postgres::{Client, NoTls};
use wp_knowledge::cache::FieldQueryCache;
use wp_knowledge::facade as kdb;
use wp_knowledge::runtime::{CachePolicy, QueryRequest, fields_to_params, runtime};
use wp_model_core::model::{DataField, Value};

fn postgres_url() -> String {
    std::env::var("WP_KDB_TEST_POSTGRES_URL")
        .expect("WP_KDB_TEST_POSTGRES_URL must be set to run postgres_provider")
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn postgres_test_guard() -> &'static Mutex<()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(()))
}

async fn connect_postgres_with_retry(url: &str) -> Client {
    let mut last_err = None;
    for _ in 0..30 {
        match tokio_postgres::connect(url, NoTls).await {
            Ok((client, connection)) => {
                tokio::spawn(async move {
                    let _ = connection.await;
                });
                return client;
            }
            Err(err) => {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
    panic!(
        "connect postgres failed: {}",
        last_err.expect("postgres connect error")
    );
}

fn seed_postgres_lookup_table(url: &str) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime for postgres seed");
    rt.block_on(async {
        let admin = connect_postgres_with_retry(url).await;
        admin
            .batch_execute(
                r#"
CREATE TABLE IF NOT EXISTS wp_knowledge_pg_lookup (
    id BIGINT PRIMARY KEY,
    name TEXT NOT NULL,
    pinying TEXT NOT NULL
);
TRUNCATE TABLE wp_knowledge_pg_lookup;
INSERT INTO wp_knowledge_pg_lookup (id, name, pinying)
VALUES
    (1, '令狐冲', 'linghuchong'),
    (2, '小龙女', 'xiaolongnu');
"#,
            )
            .await
            .expect("seed postgres lookup table failed");
    });
}

fn datafield_digit(field: &DataField) -> i64 {
    match field.get_value() {
        Value::Digit(value) => *value,
        other => panic!("expected digit field, got {other:?}"),
    }
}

fn ensure_postgres_provider_initialized() -> String {
    static INIT: OnceLock<String> = OnceLock::new();
    INIT.get_or_init(|| {
        let url = postgres_url();

        seed_postgres_lookup_table(&url);

        let root = workspace_root();
        let tmp = root.join(".tmp");
        fs::create_dir_all(&tmp).expect("create .tmp for postgres_provider failed");
        let conf_path = tmp.join("postgres-knowdb.toml");
        fs::write(
            &conf_path,
            format!(
                r#"
version = 2

[provider]
kind = "postgres"
connection_uri = "{url}"
"#
            ),
        )
        .expect("write postgres_provider knowdb config failed");

        let authority_uri = format!(
            "file:{}?mode=rwc&uri=true",
            tmp.join("unused.sqlite").display()
        );
        kdb::init_thread_cloned_from_knowdb(
            &root,
            &conf_path,
            &authority_uri,
            &orion_variate::EnvDict::default(),
        )
        .expect("init postgres provider from knowdb failed");

        url
    })
    .clone()
}

#[test]
#[ignore = "requires WP_KDB_TEST_POSTGRES_URL and a reachable PostgreSQL instance"]
fn postgres_provider_reconnects_after_backend_termination() {
    let _guard = postgres_test_guard().lock().expect("postgres test guard");
    let url = postgres_url();
    seed_postgres_lookup_table(&url);

    kdb::init_postgres_provider(&url, Some(1)).expect("init single-client postgres provider");

    let pid_row = kdb::query_row("SELECT pg_backend_pid() AS pid").expect("query backend pid");
    let pid = datafield_digit(&pid_row[0]);
    let pid_i32 = i32::try_from(pid).expect("postgres backend pid should fit int4");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime for postgres backend termination");
    rt.block_on(async {
        let admin = connect_postgres_with_retry(&url).await;
        let terminated = admin
            .query_one("SELECT pg_terminate_backend($1)", &[&pid_i32])
            .await
            .expect("terminate provider backend");
        assert!(
            terminated.get::<usize, bool>(0),
            "postgres did not terminate backend pid {pid}"
        );
    });

    let params = [DataField::from_chars(
        ":name".to_string(),
        "令狐冲".to_string(),
    )];
    let row = kdb::query_fields(
        "SELECT pinying FROM wp_knowledge_pg_lookup WHERE name=:name",
        &params,
    )
    .expect("postgres query should reconnect after backend termination");
    assert_eq!(row.len(), 1);
    assert_eq!(row[0].to_string(), "chars(linghuchong)");

    let new_pid_row =
        kdb::query_row("SELECT pg_backend_pid() AS pid").expect("query reconnected backend pid");
    let new_pid = datafield_digit(&new_pid_row[0]);
    assert_ne!(
        new_pid, pid,
        "provider reused a terminated postgres backend"
    );
}

#[test]
#[ignore = "requires WP_KDB_TEST_POSTGRES_URL and a reachable PostgreSQL instance"]
fn postgres_provider_init_and_query_inside_tokio_runtime() {
    let _guard = postgres_test_guard().lock().expect("postgres test guard");
    let url = postgres_url();
    seed_postgres_lookup_table(&url);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime for postgres provider");

    rt.block_on(async {
        kdb::init_postgres_provider(&url, Some(4))
            .expect("init postgres provider inside tokio runtime");

        let params = [DataField::from_chars(
            ":name".to_string(),
            "令狐冲".to_string(),
        )];
        let row = kdb::query_fields(
            "SELECT pinying FROM wp_knowledge_pg_lookup WHERE name=:name",
            &params,
        )
        .expect("postgres query inside tokio runtime");
        assert_eq!(row.len(), 1);
        assert_eq!(row[0].get_name(), "pinying");
        assert_eq!(row[0].to_string(), "chars(linghuchong)");
    });
}

fn perf_env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
}

fn perf_concurrency_levels() -> Vec<usize> {
    std::env::var("WP_KDB_PERF_CONCURRENCY")
        .ok()
        .map(|raw| {
            raw.split(',')
                .filter_map(|part| part.trim().parse::<usize>().ok())
                .filter(|value| *value > 0)
                .collect::<Vec<_>>()
        })
        .filter(|levels| !levels.is_empty())
        .unwrap_or_else(|| vec![1, 4, 16, 64])
}

fn seed_postgres_perf_table(url: &str, rows: usize) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime for postgres perf seed");
    rt.block_on(async {
        let admin = connect_postgres_with_retry(url).await;
        admin
            .batch_execute(
                r#"
CREATE TABLE IF NOT EXISTS wp_knowledge_pg_perf_lookup (
    id BIGINT PRIMARY KEY,
    value TEXT NOT NULL
);
TRUNCATE TABLE wp_knowledge_pg_perf_lookup;
"#,
            )
            .await
            .expect("prepare postgres perf table failed");

        let stmt = admin
            .prepare("INSERT INTO wp_knowledge_pg_perf_lookup (id, value) VALUES ($1, $2)")
            .await
            .expect("prepare postgres perf insert failed");
        for id in 1..=rows as i64 {
            let value = format!("value_{id}");
            admin
                .execute(&stmt, &[&id, &value])
                .await
                .expect("insert postgres perf row failed");
        }
    });
}

#[derive(Clone)]
struct PerfQuery {
    cache_key: [DataField; 1],
    query_params: [DataField; 1],
    bypass_params: [DataField; 1],
    global_req: QueryRequest,
}

fn build_perf_workload(ops: usize, hotset: usize) -> Vec<PerfQuery> {
    (0..ops)
        .map(|idx| {
            let id = ((idx * 17) % hotset + 1) as i64;
            let cache_key = [DataField::from_digit("id", id)];
            let query_params = [DataField::from_digit(":id", id)];
            let bypass_params = [DataField::from_digit(":id".to_string(), id)];
            let global_req = QueryRequest::first_row(
                "SELECT value FROM wp_knowledge_pg_perf_lookup WHERE id=:id",
                fields_to_params(&query_params),
                CachePolicy::UseGlobal,
            );
            PerfQuery {
                cache_key,
                query_params,
                bypass_params,
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

#[derive(Debug, Clone)]
struct LatencyResult {
    name: &'static str,
    elapsed: Duration,
    ops: usize,
    p50_us: f64,
    p95_us: f64,
}

impl LatencyResult {
    fn qps(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs == 0.0 {
            self.ops as f64
        } else {
            self.ops as f64 / secs
        }
    }
}

#[derive(Debug, Clone)]
struct ConcurrentLatencyResult {
    name: &'static str,
    concurrency: usize,
    elapsed: Duration,
    ops: usize,
    p50_us: f64,
    p95_us: f64,
    counters: PerfCounters,
}

impl ConcurrentLatencyResult {
    fn qps(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs == 0.0 {
            self.ops as f64
        } else {
            self.ops as f64 / secs
        }
    }
}

#[derive(Clone, Copy)]
enum AsyncPerfMode {
    Bypass,
    GlobalCache,
    LocalCache,
}

impl AsyncPerfMode {
    fn name(self) -> &'static str {
        match self {
            AsyncPerfMode::Bypass => "async_bypass",
            AsyncPerfMode::GlobalCache => "async_global_cache",
            AsyncPerfMode::LocalCache => "async_local_cache",
        }
    }
}

fn percentile_us(values: &[f64], numerator: usize, denominator: usize) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("latency sort"));
    let idx = ((sorted.len() - 1) * numerator) / denominator;
    sorted[idx]
}

fn shard_workload(workload: &[PerfQuery], workers: usize) -> Vec<Vec<PerfQuery>> {
    let worker_count = workers.max(1).min(workload.len().max(1));
    let mut shards = vec![Vec::new(); worker_count];
    for (idx, item) in workload.iter().cloned().enumerate() {
        shards[idx % worker_count].push(item);
    }
    shards
}

fn perf_counter_delta(before: &wp_knowledge::runtime::RuntimeSnapshot) -> PerfCounters {
    let after = kdb::runtime_snapshot();
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

fn run_postgres_bypass(url: &str, workload: &[PerfQuery]) -> PerfResult {
    kdb::init_postgres_provider(url, Some(8)).expect("init postgres provider for bypass");
    let before = kdb::runtime_snapshot();
    let started = Instant::now();
    for item in workload {
        let row = kdb::query_fields(
            "SELECT value FROM wp_knowledge_pg_perf_lookup WHERE id=:id",
            &item.bypass_params,
        )
        .expect("postgres bypass query");
        black_box(row);
    }
    PerfResult {
        name: "bypass",
        elapsed: started.elapsed(),
        ops: workload.len(),
        counters: perf_counter_delta(&before),
    }
}

fn run_postgres_global_cache(url: &str, workload: &[PerfQuery]) -> PerfResult {
    kdb::init_postgres_provider(url, Some(8)).expect("init postgres provider for global cache");
    let before = kdb::runtime_snapshot();
    let started = Instant::now();
    for item in workload {
        let row = runtime()
            .execute(&item.global_req)
            .expect("postgres global-cache query")
            .into_row();
        black_box(row);
    }
    PerfResult {
        name: "global_cache",
        elapsed: started.elapsed(),
        ops: workload.len(),
        counters: perf_counter_delta(&before),
    }
}

fn run_postgres_local_cache(url: &str, workload: &[PerfQuery]) -> PerfResult {
    kdb::init_postgres_provider(url, Some(8)).expect("init postgres provider for local cache");
    let mut cache = wp_knowledge::cache::FieldQueryCache::with_capacity(workload.len().max(1));
    let before = kdb::runtime_snapshot();
    let started = Instant::now();
    for item in workload {
        let row = kdb::cache_query_fields(
            "SELECT value FROM wp_knowledge_pg_perf_lookup WHERE id=:id",
            &item.cache_key,
            &item.query_params,
            &mut cache,
        );
        black_box(row);
    }
    PerfResult {
        name: "local_cache",
        elapsed: started.elapsed(),
        ops: workload.len(),
        counters: perf_counter_delta(&before),
    }
}

fn print_perf_result(result: &PerfResult) {
    eprintln!(
        "[wp-knowledge][postgres-cache-perf] scenario={} elapsed_ms={} qps={:.0} result_hit={} result_miss={} local_hit={} local_miss={} metadata_hit={} metadata_miss={}",
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

fn print_latency_result(provider: &str, result: &LatencyResult) {
    eprintln!(
        "[wp-knowledge][{provider}-sync-async] mode={} elapsed_ms={} qps={:.0} p50_us={:.2} p95_us={:.2}",
        result.name,
        result.elapsed.as_millis(),
        result.qps(),
        result.p50_us,
        result.p95_us,
    );
}

fn print_concurrent_latency_result(provider: &str, result: &ConcurrentLatencyResult) {
    eprintln!(
        "[wp-knowledge][{provider}-async-cache-concurrency] mode={} concurrency={} elapsed_ms={} qps={:.0} p50_us={:.2} p95_us={:.2} result_hit={} result_miss={} local_hit={} local_miss={} metadata_hit={} metadata_miss={}",
        result.name,
        result.concurrency,
        result.elapsed.as_millis(),
        result.qps(),
        result.p50_us,
        result.p95_us,
        result.counters.result_hits,
        result.counters.result_misses,
        result.counters.local_hits,
        result.counters.local_misses,
        result.counters.metadata_hits,
        result.counters.metadata_misses,
    );
}

fn run_postgres_sync_latency(url: &str, workload: &[PerfQuery]) -> LatencyResult {
    kdb::init_postgres_provider(url, Some(8)).expect("init postgres provider for sync latency");
    let warm = [DataField::from_digit(":id", 1)];
    let _ = kdb::query_fields(
        "SELECT value FROM wp_knowledge_pg_perf_lookup WHERE id=:id",
        &warm,
    )
    .expect("warm postgres sync query");

    let mut samples_us = Vec::with_capacity(workload.len());
    let started = Instant::now();
    for item in workload {
        let op_started = Instant::now();
        let row = kdb::query_fields(
            "SELECT value FROM wp_knowledge_pg_perf_lookup WHERE id=:id",
            &item.bypass_params,
        )
        .expect("postgres sync query");
        black_box(row);
        samples_us.push(op_started.elapsed().as_secs_f64() * 1_000_000.0);
    }

    LatencyResult {
        name: "sync",
        elapsed: started.elapsed(),
        ops: workload.len(),
        p50_us: percentile_us(&samples_us, 50, 100),
        p95_us: percentile_us(&samples_us, 95, 100),
    }
}

fn run_postgres_async_latency(url: &str, workload: &[PerfQuery]) -> LatencyResult {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime for postgres async latency");
    rt.block_on(async {
        kdb::init_postgres_provider(url, Some(8))
            .expect("init postgres provider for async latency");
        let warm = [DataField::from_digit(":id", 1)];
        let _ = kdb::query_fields_async(
            "SELECT value FROM wp_knowledge_pg_perf_lookup WHERE id=:id",
            &warm,
        )
        .await
        .expect("warm postgres async query");

        let mut samples_us = Vec::with_capacity(workload.len());
        let started = Instant::now();
        for item in workload {
            let op_started = Instant::now();
            let row = kdb::query_fields_async(
                "SELECT value FROM wp_knowledge_pg_perf_lookup WHERE id=:id",
                &item.bypass_params,
            )
            .await
            .expect("postgres async query");
            black_box(row);
            samples_us.push(op_started.elapsed().as_secs_f64() * 1_000_000.0);
        }

        LatencyResult {
            name: "async",
            elapsed: started.elapsed(),
            ops: workload.len(),
            p50_us: percentile_us(&samples_us, 50, 100),
            p95_us: percentile_us(&samples_us, 95, 100),
        }
    })
}

fn run_postgres_async_cache_concurrency(
    url: &str,
    workload: &[PerfQuery],
    concurrency: usize,
    mode: AsyncPerfMode,
) -> ConcurrentLatencyResult {
    kdb::init_postgres_provider(url, Some(8))
        .expect("init postgres provider for async cache concurrency");
    let worker_count = concurrency.max(1).min(workload.len().max(1));
    let worker_threads = worker_count.clamp(2, 16);
    let shards = shard_workload(workload, worker_count);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .build()
        .expect("build tokio runtime for postgres async cache concurrency");

    rt.block_on(async move {
        match mode {
            AsyncPerfMode::Bypass => {
                let warm = [DataField::from_digit(":id", 1)];
                let _ = kdb::query_fields_async(
                    "SELECT value FROM wp_knowledge_pg_perf_lookup WHERE id=:id",
                    &warm,
                )
                .await
                .expect("warm postgres async bypass query");
            }
            AsyncPerfMode::GlobalCache => {
                let req = QueryRequest::first_row(
                    "SELECT value FROM wp_knowledge_pg_perf_lookup WHERE id=:id",
                    fields_to_params(&[DataField::from_digit(":id", 1)]),
                    CachePolicy::UseGlobal,
                );
                let _ = runtime()
                    .execute_async(&req)
                    .await
                    .expect("warm postgres async global-cache query");
            }
            AsyncPerfMode::LocalCache => {
                let mut cache = FieldQueryCache::with_capacity(16);
                let cache_key = [DataField::from_digit("id", 1)];
                let query_params = [DataField::from_digit(":id", 1)];
                let _ = kdb::cache_query_fields_async(
                    "SELECT value FROM wp_knowledge_pg_perf_lookup WHERE id=:id",
                    &cache_key,
                    &query_params,
                    &mut cache,
                )
                .await;
            }
        }

        let before = kdb::runtime_snapshot();
        let barrier = std::sync::Arc::new(Barrier::new(worker_count + 1));
        let mut set = JoinSet::new();
        for shard in shards {
            let barrier = barrier.clone();
            set.spawn(async move {
                let mut samples_us = Vec::with_capacity(shard.len());
                let mut cache = FieldQueryCache::with_capacity(shard.len().max(1));
                barrier.wait().await;
                for item in shard {
                    let op_started = Instant::now();
                    let row = match mode {
                        AsyncPerfMode::Bypass => kdb::query_fields_async(
                            "SELECT value FROM wp_knowledge_pg_perf_lookup WHERE id=:id",
                            &item.bypass_params,
                        )
                        .await
                        .expect("postgres async bypass query"),
                        AsyncPerfMode::GlobalCache => runtime()
                            .execute_async(&item.global_req)
                            .await
                            .expect("postgres async global-cache query")
                            .into_row(),
                        AsyncPerfMode::LocalCache => {
                            kdb::cache_query_fields_async(
                                "SELECT value FROM wp_knowledge_pg_perf_lookup WHERE id=:id",
                                &item.cache_key,
                                &item.query_params,
                                &mut cache,
                            )
                            .await
                        }
                    };
                    black_box(row);
                    samples_us.push(op_started.elapsed().as_secs_f64() * 1_000_000.0);
                }
                samples_us
            });
        }

        let started = Instant::now();
        barrier.wait().await;
        let mut samples_us = Vec::with_capacity(workload.len());
        while let Some(joined) = set.join_next().await {
            samples_us.extend(joined.expect("join postgres async cache worker"));
        }
        let elapsed = started.elapsed();
        let counters = perf_counter_delta(&before);
        ConcurrentLatencyResult {
            name: mode.name(),
            concurrency: worker_count,
            elapsed,
            ops: workload.len(),
            p50_us: percentile_us(&samples_us, 50, 100),
            p95_us: percentile_us(&samples_us, 95, 100),
            counters,
        }
    })
}

#[test]
#[ignore = "requires WP_KDB_TEST_POSTGRES_URL and a reachable PostgreSQL instance"]
fn postgres_provider_query_and_pool() {
    let _guard = postgres_test_guard().lock().expect("postgres test guard");
    let _url = ensure_postgres_provider_initialized();

    let before = kdb::runtime_snapshot();
    let params = [DataField::from_chars(
        ":name".to_string(),
        "令狐冲".to_string(),
    )];
    let row = kdb::query_fields(
        "SELECT pinying FROM wp_knowledge_pg_lookup WHERE name=:name",
        &params,
    )
    .expect("postgres named query");
    assert_eq!(row.len(), 1);
    assert_eq!(row[0].get_name(), "pinying");
    assert_eq!(row[0].to_string(), "chars(linghuchong)");
    let row = kdb::query_fields(
        "SELECT pinying FROM wp_knowledge_pg_lookup WHERE name=:name",
        &params,
    )
    .expect("postgres named query second hit");
    assert_eq!(row[0].to_string(), "chars(linghuchong)");
    let after_metadata = kdb::runtime_snapshot();
    assert!(after_metadata.metadata_cache_misses > before.metadata_cache_misses);
    assert!(after_metadata.metadata_cache_hits > before.metadata_cache_hits);

    let before_empty = kdb::runtime_snapshot();
    let empty = kdb::query_fields(
        "SELECT pinying FROM wp_knowledge_pg_lookup WHERE name=:name AND id=-1",
        &params,
    )
    .expect("postgres empty metadata miss");
    assert!(empty.is_empty());
    let empty = kdb::query_fields(
        "SELECT pinying FROM wp_knowledge_pg_lookup WHERE name=:name AND id=-1",
        &params,
    )
    .expect("postgres empty metadata hit");
    assert!(empty.is_empty());
    let after_empty = kdb::runtime_snapshot();
    assert!(after_empty.metadata_cache_misses > before_empty.metadata_cache_misses);
    assert!(after_empty.metadata_cache_hits > before_empty.metadata_cache_hits);

    let started = Instant::now();
    thread::scope(|scope| {
        for _ in 0..6 {
            scope.spawn(|| {
                let params = [DataField::from_chars(
                    ":name".to_string(),
                    "令狐冲".to_string(),
                )];
                let row = kdb::query_fields(
                    r#"
WITH wait_for_pool AS (SELECT pg_sleep(0.2))
SELECT pinying
FROM wp_knowledge_pg_lookup
CROSS JOIN wait_for_pool
WHERE name=:name
"#,
                    &params,
                )
                .expect("concurrent postgres query");
                assert_eq!(row[0].to_string(), "chars(linghuchong)");
            });
        }
    });

    assert!(
        started.elapsed() < Duration::from_millis(900),
        "postgres pooled queries look serialized: {:?}",
        started.elapsed()
    );
}

#[test]
#[ignore = "requires WP_KDB_TEST_POSTGRES_URL and a reachable PostgreSQL instance"]
fn postgres_provider_sqlx_type_compatibility() {
    let _guard = postgres_test_guard().lock().expect("postgres test guard");
    let url = ensure_postgres_provider_initialized();

    kdb::init_postgres_provider(&url, Some(4))
        .expect("init postgres provider for type compatibility");

    let count = kdb::query_row("SELECT COUNT(*) AS row_count FROM wp_knowledge_pg_lookup")
        .expect("query postgres count");
    assert_eq!(datafield_digit(&count[0]), 2);

    let row = kdb::query_row("SELECT 'pg_class'::regclass::oid AS class_oid")
        .expect("query postgres oid");
    assert_eq!(row[0].get_name(), "class_oid");
    assert!(
        datafield_digit(&row[0]) > 0,
        "postgres oid should decode as a positive digit"
    );

    let row = kdb::query_row("SELECT 12345.6789::numeric(20, 4) AS amount")
        .expect("query postgres numeric");
    assert_eq!(row[0].get_name(), "amount");
    assert_eq!(row[0].to_string(), "chars(12345.6789)");
}

#[test]
#[ignore = "requires WP_KDB_TEST_POSTGRES_URL and a reachable PostgreSQL instance"]
fn postgres_provider_cache_perf() {
    let _guard = postgres_test_guard().lock().expect("postgres test guard");
    let url = postgres_url();
    let rows = perf_env_usize("WP_KDB_PERF_ROWS", 10_000).max(1);
    let ops = perf_env_usize("WP_KDB_PERF_OPS", 10_000).max(1);
    let hotset = perf_env_usize("WP_KDB_PERF_HOTSET", 128).clamp(1, rows);
    seed_postgres_perf_table(&url, rows);
    let workload = build_perf_workload(ops, hotset);

    eprintln!(
        "[wp-knowledge][postgres-cache-perf] rows={} ops={} hotset={} sql=SELECT value FROM wp_knowledge_pg_perf_lookup WHERE id=:id",
        rows, ops, hotset
    );

    let bypass = run_postgres_bypass(&url, &workload);
    let global = run_postgres_global_cache(&url, &workload);
    let local = run_postgres_local_cache(&url, &workload);

    print_perf_result(&bypass);
    print_perf_result(&global);
    print_perf_result(&local);

    eprintln!(
        "[wp-knowledge][postgres-cache-perf] speedup global_vs_bypass={:.2}x local_vs_bypass={:.2}x",
        bypass.elapsed.as_secs_f64() / global.elapsed.as_secs_f64(),
        bypass.elapsed.as_secs_f64() / local.elapsed.as_secs_f64(),
    );

    assert_eq!(bypass.counters.result_hits, 0);
    assert_eq!(bypass.counters.result_misses, 0);
    assert_eq!(bypass.counters.local_hits, 0);
    assert_eq!(bypass.counters.local_misses, 0);
    assert!(bypass.counters.metadata_hits > 0);
    assert!(bypass.counters.metadata_misses > 0);
    assert!(global.counters.result_hits > 0);
    assert!(global.counters.result_misses > 0);
    assert_eq!(global.counters.local_hits, 0);
    assert_eq!(global.counters.local_misses, 0);
    assert!(global.counters.metadata_hits > 0);
    assert!(global.counters.metadata_misses > 0);
    assert!(local.counters.local_hits > 0);
    assert!(local.counters.local_misses > 0);
    assert!(local.counters.result_misses > 0);
    assert!(local.counters.metadata_hits > 0);
    assert!(local.counters.metadata_misses > 0);
}

#[test]
#[ignore = "requires WP_KDB_TEST_POSTGRES_URL and a reachable PostgreSQL instance"]
fn postgres_provider_sync_vs_async_perf() {
    let _guard = postgres_test_guard().lock().expect("postgres test guard");
    let url = postgres_url();
    let rows = perf_env_usize("WP_KDB_PERF_ROWS", 10_000).max(1);
    let ops = perf_env_usize("WP_KDB_PERF_OPS", 10_000).max(1);
    let hotset = perf_env_usize("WP_KDB_PERF_HOTSET", 128).clamp(1, rows);
    seed_postgres_perf_table(&url, rows);
    let workload = build_perf_workload(ops, hotset);

    let sync = run_postgres_sync_latency(&url, &workload);
    let async_result = run_postgres_async_latency(&url, &workload);

    eprintln!(
        "[wp-knowledge][postgres-sync-async] rows={} ops={} hotset={} sql=SELECT value FROM wp_knowledge_pg_perf_lookup WHERE id=:id",
        rows, ops, hotset
    );
    print_latency_result("postgres", &sync);
    print_latency_result("postgres", &async_result);
}

#[test]
#[ignore = "requires WP_KDB_TEST_POSTGRES_URL and a reachable PostgreSQL instance"]
fn postgres_provider_async_cache_concurrency_perf() {
    let _guard = postgres_test_guard().lock().expect("postgres test guard");
    let url = postgres_url();
    let rows = perf_env_usize("WP_KDB_PERF_ROWS", 10_000).max(1);
    let ops = perf_env_usize("WP_KDB_PERF_OPS", 20_000).max(1);
    let hotset = perf_env_usize("WP_KDB_PERF_HOTSET", 128).clamp(1, rows);
    seed_postgres_perf_table(&url, rows);
    let workload = build_perf_workload(ops, hotset);

    eprintln!(
        "[wp-knowledge][postgres-async-cache-concurrency] rows={} ops={} hotset={} concurrencies={:?}",
        rows,
        ops,
        hotset,
        perf_concurrency_levels()
    );

    for concurrency in perf_concurrency_levels() {
        for mode in [
            AsyncPerfMode::Bypass,
            AsyncPerfMode::GlobalCache,
            AsyncPerfMode::LocalCache,
        ] {
            let result = run_postgres_async_cache_concurrency(&url, &workload, concurrency, mode);
            print_concurrent_latency_result("postgres", &result);
        }
    }
}
