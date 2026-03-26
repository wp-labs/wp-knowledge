use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use postgres::{Client, NoTls};
use wp_knowledge::facade as kdb;
use wp_knowledge::runtime::{CachePolicy, QueryRequest, fields_to_params, runtime};
use wp_model_core::model::DataField;

fn postgres_url() -> String {
    std::env::var("WP_KDB_TEST_POSTGRES_URL")
        .expect("WP_KDB_TEST_POSTGRES_URL must be set to run postgres_provider")
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn ensure_postgres_provider_initialized() -> String {
    static INIT: OnceLock<String> = OnceLock::new();
    INIT.get_or_init(|| {
        let url = postgres_url();

        let mut admin =
            Client::connect(&url, NoTls).expect("connect to WP_KDB_TEST_POSTGRES_URL failed");
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
            .expect("seed postgres_provider test data failed");

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

fn perf_env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
}

fn seed_postgres_perf_table(url: &str, rows: usize) {
    let mut admin = Client::connect(url, NoTls).expect("connect postgres perf admin failed");
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
        .expect("prepare postgres perf table failed");

    let stmt = admin
        .prepare("INSERT INTO wp_knowledge_pg_perf_lookup (id, value) VALUES ($1, $2)")
        .expect("prepare postgres perf insert failed");
    for id in 1..=rows as i64 {
        admin
            .execute(&stmt, &[&id, &format!("value_{id}")])
            .expect("insert postgres perf row failed");
    }
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

#[test]
#[ignore = "requires WP_KDB_TEST_POSTGRES_URL and a reachable PostgreSQL instance"]
fn postgres_provider_query_and_pool() {
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
fn postgres_provider_cache_perf() {
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
