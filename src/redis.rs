use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::error::{KnowReason, KnowledgeResult};
use orion_error::conversion::ToStructError;
use redis::aio::ConnectionManager;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

#[allow(dead_code)]
const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 100;

#[allow(dead_code)]
const SUPPORTED_COMMANDS: &[&str] = &[
    "BF.EXISTS",  // P0
    "HGET",       // P0
    "GET",        // P0
    "SISMEMBER",  // P0
    "BF.MADD",    // P1
    "BF.RESERVE", // P1
];

// ---------------------------------------------------------------------------
// RedisPool — wraps ConnectionManager with timeout config
// ---------------------------------------------------------------------------

struct RedisPool {
    conn: ConnectionManager,
    command_timeout: Duration,
}

impl RedisPool {
    fn timeout(&self) -> Duration {
        self.command_timeout
    }
}

// ---------------------------------------------------------------------------
// Global Registry
// ---------------------------------------------------------------------------

struct RedisRegistry {
    /// provider_name → endpoint_url
    names: HashMap<String, String>,
    /// endpoint_url → shared pool
    pools: HashMap<String, Arc<RedisPool>>,
}

impl RedisRegistry {
    fn new() -> Self {
        Self {
            names: HashMap::new(),
            pools: HashMap::new(),
        }
    }

    fn register(&mut self, name: &str, url: &str, pool: Arc<RedisPool>) -> KnowledgeResult<()> {
        if self.names.contains_key(name) {
            return Err(KnowReason::from_conf()
                .to_err()
                .with_detail(format!("redis provider '{name}' already registered")));
        }
        self.names.insert(name.to_string(), url.to_string());
        // Dedup: only insert pool if URL not already present
        self.pools.entry(url.to_string()).or_insert(pool);
        Ok(())
    }

    fn resolve(&self, name: &str) -> KnowledgeResult<Arc<RedisPool>> {
        let url = self.names.get(name).ok_or_else(|| {
            KnowReason::from_logic()
                .to_err()
                .with_detail(format!("redis provider '{name}' not found"))
        })?;
        self.pools.get(url).cloned().ok_or_else(|| {
            KnowReason::from_logic()
                .to_err()
                .with_detail(format!("redis pool for '{name}' (url={url}) missing"))
        })
    }

    #[allow(dead_code)]
    fn remove(&mut self, name: &str) {
        let url = match self.names.remove(name) {
            Some(u) => u,
            None => return,
        };
        // Only drop the pool if no other name references this URL
        if !self.names.values().any(|v| v == &url) {
            self.pools.remove(&url);
        }
    }

    #[allow(dead_code)]
    fn remove_all(&mut self) {
        self.names.clear();
        self.pools.clear();
    }
}

fn registry() -> &'static Mutex<RedisRegistry> {
    static REGISTRY: OnceLock<Mutex<RedisRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(RedisRegistry::new()))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn cmd_err(cmd: &str, name: &str, err: redis::RedisError) -> crate::error::KnowledgeError {
    KnowReason::from_logic()
        .to_err()
        .with_detail(format!("redis command '{cmd}' on '{name}' failed: {err}"))
}

fn timeout_err(cmd: &str, name: &str, timeout: Duration) -> crate::error::KnowledgeError {
    KnowReason::from_logic().to_err().with_detail(format!(
        "redis command '{cmd}' on '{name}' timed out after {}ms",
        timeout.as_millis()
    ))
}

/// Resolve pool and clone a ConnectionManager (lock held only briefly).
fn resolve_pool(name: &str) -> KnowledgeResult<(Duration, ConnectionManager)> {
    let pool = registry()
        .lock()
        .expect("redis registry lock poisoned")
        .resolve(name)?;
    let timeout = pool.timeout();
    let conn = pool.conn.clone();
    Ok((timeout, conn))
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub(crate) fn init(name: &str, url: &str, pool_size: Option<usize>) -> KnowledgeResult<()> {
    init_with_opts(name, url, pool_size, DEFAULT_COMMAND_TIMEOUT_MS)
}

pub(crate) fn init_with_opts(
    name: &str,
    url: &str,
    pool_size: Option<usize>,
    command_timeout_ms: u64,
) -> KnowledgeResult<()> {
    // Validate URL scheme
    if !url.starts_with("redis://") && !url.starts_with("rediss://") {
        return Err(KnowReason::from_conf()
            .to_err()
            .with_detail(format!("invalid redis url: '{url}'")));
    }

    // pool_size is reserved for future use; ConnectionManager handles its own
    // connection pool internally.
    let _ = pool_size;

    // Fast path: pool already exists for this URL
    {
        let reg = registry().lock().expect("redis registry lock poisoned");
        if let Some(existing) = reg.pools.get(url) {
            let existing = existing.clone();
            drop(reg);
            let mut reg = registry().lock().expect("redis registry lock poisoned");
            return reg.register(name, url, existing);
        }
    }

    // Build a new ConnectionManager
    let client = redis::Client::open(url).map_err(|err| {
        KnowReason::from_conf()
            .to_err()
            .with_detail(format!("redis client create failed for '{url}': {err}"))
    })?;

    let conn = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            ConnectionManager::new(client).await.map_err(|err| {
                KnowReason::from_conf()
                    .to_err()
                    .with_detail(format!("redis connect failed for '{url}': {err}"))
            })
        })
    })?;

    let pool = Arc::new(RedisPool {
        conn,
        command_timeout: Duration::from_millis(command_timeout_ms),
    });

    let mut reg = registry().lock().expect("redis registry lock poisoned");
    // Re-check: another thread may have registered a pool for this URL while
    // we were connecting. Use the existing pool if so.
    let existing = reg.pools.get(url).cloned();
    if let Some(existing) = existing {
        return reg.register(name, url, existing);
    }
    reg.register(name, url, pool)
}

// ---------------------------------------------------------------------------
// Typed async commands — facade calls these directly (no String round-trip)
// ---------------------------------------------------------------------------

pub(crate) async fn bf_exists_async(name: &str, key: &str, item: &str) -> KnowledgeResult<bool> {
    let (timeout, mut conn) = resolve_pool(name)?;
    let fut = async {
        let exists: bool = redis::cmd("BF.EXISTS")
            .arg(key)
            .arg(item)
            .query_async(&mut conn)
            .await?;
        Ok::<bool, redis::RedisError>(exists)
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(cmd_err("BF.EXISTS", name, e)),
        Err(_) => Err(timeout_err("BF.EXISTS", name, timeout)),
    }
}

pub(crate) async fn hget_async(
    name: &str,
    key: &str,
    field: &str,
) -> KnowledgeResult<Option<String>> {
    let (timeout, mut conn) = resolve_pool(name)?;
    let fut = async {
        let value: Option<String> = redis::cmd("HGET")
            .arg(key)
            .arg(field)
            .query_async(&mut conn)
            .await?;
        Ok::<Option<String>, redis::RedisError>(value)
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(cmd_err("HGET", name, e)),
        Err(_) => Err(timeout_err("HGET", name, timeout)),
    }
}

pub(crate) async fn get_async(name: &str, key: &str) -> KnowledgeResult<Option<String>> {
    let (timeout, mut conn) = resolve_pool(name)?;
    let fut = async {
        let value: Option<String> = redis::cmd("GET").arg(key).query_async(&mut conn).await?;
        Ok::<Option<String>, redis::RedisError>(value)
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(cmd_err("GET", name, e)),
        Err(_) => Err(timeout_err("GET", name, timeout)),
    }
}

pub(crate) async fn set_exists_async(name: &str, key: &str, member: &str) -> KnowledgeResult<bool> {
    let (timeout, mut conn) = resolve_pool(name)?;
    let fut = async {
        let is_member: bool = redis::cmd("SISMEMBER")
            .arg(key)
            .arg(member)
            .query_async(&mut conn)
            .await?;
        Ok::<bool, redis::RedisError>(is_member)
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(cmd_err("SISMEMBER", name, e)),
        Err(_) => Err(timeout_err("SISMEMBER", name, timeout)),
    }
}

pub(crate) async fn bf_madd_async(
    name: &str,
    key: &str,
    items: &[String],
) -> KnowledgeResult<Vec<bool>> {
    let (timeout, mut conn) = resolve_pool(name)?;
    let fut = async {
        let mut bloom_cmd = redis::cmd("BF.MADD");
        bloom_cmd.arg(key);
        for item in items {
            bloom_cmd.arg(item);
        }
        let results: Vec<bool> = bloom_cmd.query_async(&mut conn).await?;
        Ok::<Vec<bool>, redis::RedisError>(results)
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(cmd_err("BF.MADD", name, e)),
        Err(_) => Err(timeout_err("BF.MADD", name, timeout)),
    }
}

pub(crate) async fn bf_reserve_async(
    name: &str,
    key: &str,
    error_rate: f64,
    capacity: i64,
) -> KnowledgeResult<()> {
    let (timeout, mut conn) = resolve_pool(name)?;
    let fut = async {
        let _ok: String = redis::cmd("BF.RESERVE")
            .arg(key)
            .arg(error_rate)
            .arg(capacity)
            .query_async(&mut conn)
            .await?;
        Ok::<(), redis::RedisError>(())
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(cmd_err("BF.RESERVE", name, e)),
        Err(_) => Err(timeout_err("BF.RESERVE", name, timeout)),
    }
}

// ---------------------------------------------------------------------------
// Typed blocking wrappers
// ---------------------------------------------------------------------------

pub(crate) fn bf_exists(name: &str, key: &str, item: &str) -> KnowledgeResult<bool> {
    block_on(bf_exists_async(name, key, item))
}

pub(crate) fn hget(name: &str, key: &str, field: &str) -> KnowledgeResult<Option<String>> {
    block_on(hget_async(name, key, field))
}

pub(crate) fn get(name: &str, key: &str) -> KnowledgeResult<Option<String>> {
    block_on(get_async(name, key))
}

pub(crate) fn set_exists(name: &str, key: &str, member: &str) -> KnowledgeResult<bool> {
    block_on(set_exists_async(name, key, member))
}

pub(crate) fn bf_madd(name: &str, key: &str, items: &[String]) -> KnowledgeResult<Vec<bool>> {
    block_on(bf_madd_async(name, key, items))
}

pub(crate) fn bf_reserve(
    name: &str,
    key: &str,
    error_rate: f64,
    capacity: i64,
) -> KnowledgeResult<()> {
    block_on(bf_reserve_async(name, key, error_rate, capacity))
}

fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}

// ---------------------------------------------------------------------------
// Generic exec (kept for tests; pub(crate))
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub(crate) async fn exec_async(
    name: &str,
    cmd: &str,
    key: &str,
    args: &[String],
) -> KnowledgeResult<String> {
    let cmd_upper = cmd.to_uppercase();
    if !SUPPORTED_COMMANDS.contains(&cmd_upper.as_str()) {
        return Err(KnowReason::from_logic()
            .to_err()
            .with_detail(format!("unsupported redis command: '{cmd}'")));
    }

    // Validate arg count before pool lookup (fail fast without touching registry)
    let min_args = match cmd_upper.as_str() {
        "GET" => 0,
        "BF.EXISTS" | "HGET" | "SISMEMBER" => 1,
        "BF.MADD" => 1,
        "BF.RESERVE" => 0,
        _ => 0,
    };
    if args.len() < min_args {
        return Err(KnowReason::from_logic()
            .to_err()
            .with_detail(format!("{cmd} requires at least {min_args} argument(s)")));
    }

    let (timeout, mut conn) = resolve_pool(name)?;

    // Wrap the Redis command in a timeout. The async block ensures tokio can
    // cancel the in-flight command when the timeout fires.
    let exec_future = async move {
        match cmd_upper.as_str() {
            "BF.EXISTS" => {
                let item = &args[0];
                let exists: bool = redis::cmd("BF.EXISTS")
                    .arg(key)
                    .arg(item)
                    .query_async(&mut conn)
                    .await?;
                Ok::<String, redis::RedisError>(if exists {
                    "1".to_string()
                } else {
                    "0".to_string()
                })
            }
            "HGET" => {
                let field = &args[0];
                let value: Option<String> = redis::cmd("HGET")
                    .arg(key)
                    .arg(field)
                    .query_async(&mut conn)
                    .await?;
                Ok(value.unwrap_or_default())
            }
            "GET" => {
                let value: Option<String> =
                    redis::cmd("GET").arg(key).query_async(&mut conn).await?;
                Ok(value.unwrap_or_default())
            }
            "SISMEMBER" => {
                let member = &args[0];
                let is_member: bool = redis::cmd("SISMEMBER")
                    .arg(key)
                    .arg(member)
                    .query_async(&mut conn)
                    .await?;
                Ok(if is_member {
                    "1".to_string()
                } else {
                    "0".to_string()
                })
            }
            "BF.MADD" => {
                let mut bloom_cmd = redis::cmd("BF.MADD");
                bloom_cmd.arg(key);
                for item in args {
                    bloom_cmd.arg(item);
                }
                let results: Vec<bool> = bloom_cmd.query_async(&mut conn).await?;
                if results.is_empty() {
                    Ok(String::new())
                } else {
                    Ok(results
                        .iter()
                        .map(|b| if *b { "1" } else { "0" })
                        .collect::<Vec<_>>()
                        .join(","))
                }
            }
            "BF.RESERVE" => {
                let error_rate: f64 =
                    args.first().and_then(|s| s.parse().ok()).ok_or_else(|| {
                        redis::RedisError::from((
                            redis::ErrorKind::TypeError,
                            "BF.RESERVE: invalid error_rate",
                        ))
                    })?;
                let capacity: i64 = args.get(1).and_then(|s| s.parse().ok()).ok_or_else(|| {
                    redis::RedisError::from((
                        redis::ErrorKind::TypeError,
                        "BF.RESERVE: invalid capacity",
                    ))
                })?;
                let ok: String = redis::cmd("BF.RESERVE")
                    .arg(key)
                    .arg(error_rate)
                    .arg(capacity)
                    .query_async(&mut conn)
                    .await?;
                Ok(ok)
            }
            _ => unreachable!(), // already validated
        }
    };

    match tokio::time::timeout(timeout, exec_future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(cmd_err(cmd, name, e)),
        Err(_) => Err(timeout_err(cmd, name, timeout)),
    }
}

#[allow(dead_code)]
pub(crate) fn exec_blocking(
    name: &str,
    cmd: &str,
    key: &str,
    args: &[String],
) -> KnowledgeResult<String> {
    block_on(exec_async(name, cmd, key, args))
}

// ---------------------------------------------------------------------------
// Ping / Close (pub(crate))
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub(crate) fn ping_blocking(name: &str) -> KnowledgeResult<bool> {
    block_on(async {
        let (timeout, mut conn) = resolve_pool(name)?;

        let ping_future = async {
            let result: String = redis::cmd("PING").query_async(&mut conn).await?;
            Ok::<_, redis::RedisError>(result == "PONG")
        };

        match tokio::time::timeout(timeout, ping_future).await {
            Ok(Ok(pong)) => Ok(pong),
            Ok(Err(e)) => Err(KnowReason::from_logic()
                .to_err()
                .with_detail(format!("redis ping failed for '{name}': {e}"))),
            Err(_elapsed) => Err(KnowReason::from_logic()
                .to_err()
                .with_detail(format!("redis ping timed out for '{name}'"))),
        }
    })
}

#[allow(dead_code)]
pub(crate) fn close(name: Option<&str>) -> KnowledgeResult<()> {
    let mut reg = registry().lock().expect("redis registry lock poisoned");
    match name {
        Some(name) => {
            reg.remove(name);
        }
        None => {
            reg.remove_all();
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: skip tests when Redis is not available.
    /// Set `WP_REDIS_URL` to enable Redis tests, e.g.:
    ///   WP_REDIS_URL=redis://127.0.0.1:6379 cargo test -p wp-knowledge -- redis
    fn redis_url() -> Option<String> {
        std::env::var("WP_REDIS_URL").ok()
    }

    fn test_name() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CNT: AtomicU64 = AtomicU64::new(0);
        format!("wpk_redis_test_{}", CNT.fetch_add(1, Ordering::Relaxed))
    }

    // Tests below use init / ping_blocking / exec_blocking which internally call
    // block_in_place + Handle::current().block_on(). They require a multi-thread
    // tokio runtime and a running Redis server (set WP_REDIS_URL env var).
    #[tokio::test(flavor = "multi_thread")]
    async fn init_and_ping() {
        let url = match redis_url() {
            Some(u) => u,
            None => return,
        };
        let name = test_name();
        init(&name, &url, None).expect("init");
        let ok = ping_blocking(&name).expect("ping");
        assert!(ok);
        close(Some(&name)).expect("close");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exec_get_set() {
        let url = match redis_url() {
            Some(u) => u,
            None => return,
        };
        let name = test_name();
        init(&name, &url, None).expect("init");

        let result = exec_blocking(&name, "GET", "_wpk_test_nonexistent", &[]).expect("get");
        assert!(
            result.is_empty(),
            "nonexistent key should return empty string"
        );

        close(Some(&name)).expect("close");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unsupported_command_returns_error() {
        let url = match redis_url() {
            Some(u) => u,
            None => return,
        };
        let name = test_name();
        init(&name, &url, None).expect("init");

        let err = exec_blocking(&name, "SET", "k", &["v".to_string()]).expect_err("unsupported");
        let msg = err.to_string();
        assert!(msg.contains("unsupported") && msg.contains("SET"));

        close(Some(&name)).expect("close");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn not_found_provider_returns_error() {
        let err = exec_async("_nonexistent_", "GET", "k", &[])
            .await
            .expect_err("not found");
        let msg = err.to_string();
        assert!(msg.contains("not found") || msg.contains("_nonexistent_"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn duplicate_init_returns_error() {
        let url = match redis_url() {
            Some(u) => u,
            None => return,
        };
        let name = test_name();
        init(&name, &url, None).expect("first init");
        let err = init(&name, &url, None).expect_err("duplicate init");
        let msg = err.to_string();
        assert!(msg.contains("already registered"));
        close(Some(&name)).expect("close");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn url_dedup_shares_pool() {
        let url = match redis_url() {
            Some(u) => u,
            None => return,
        };
        let name1 = test_name();
        let name2 = test_name();
        init(&name1, &url, None).expect("init name1");
        init(&name2, &url, None).expect("init name2");

        let ok1 = ping_blocking(&name1).expect("ping name1");
        let ok2 = ping_blocking(&name2).expect("ping name2");
        assert!(ok1);
        assert!(ok2);

        // Close name1 only — name2 should still work
        close(Some(&name1)).expect("close name1");
        let ok2 = ping_blocking(&name2).expect("ping name2 after close name1");
        assert!(ok2);

        close(Some(&name2)).expect("close name2");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn close_all_clears_all() {
        let url = match redis_url() {
            Some(u) => u,
            None => return,
        };
        let name1 = test_name();
        let name2 = test_name();
        init(&name1, &url, None).expect("init name1");
        init(&name2, &url, None).expect("init name2");

        close(None).expect("close all");

        let err1 = ping_blocking(&name1).expect_err("name1 should be gone");
        let err2 = ping_blocking(&name2).expect_err("name2 should be gone");
        assert!(err1.to_string().contains("not found"));
        assert!(err2.to_string().contains("not found"));
    }

    // ---------------------------------------------------------------------------
    // Tests that run without Redis (CI-safe)
    // ---------------------------------------------------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn unsupported_command_before_pool_lookup() {
        // Command validation happens before registry lookup — no Redis needed.
        let err = exec_async("_nonexistent_", "SET", "k", &["v".to_string()])
            .await
            .expect_err("unsupported SET should fail");
        let msg = err.to_string();
        assert!(msg.contains("unsupported") && msg.contains("SET"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bf_exists_missing_args() {
        let err = exec_async("_nonexistent_", "BF.EXISTS", "k", &[])
            .await
            .expect_err("BF.EXISTS without args should fail");
        let msg = err.to_string();
        assert!(msg.contains("requires at least 1 argument"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hget_missing_args() {
        let err = exec_async("_nonexistent_", "HGET", "k", &[])
            .await
            .expect_err("HGET without args should fail");
        let msg = err.to_string();
        assert!(msg.contains("requires at least 1 argument"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sismember_missing_args() {
        let err = exec_async("_nonexistent_", "SISMEMBER", "k", &[])
            .await
            .expect_err("SISMEMBER without args should fail");
        let msg = err.to_string();
        assert!(msg.contains("requires at least 1 argument"));
    }

    #[test]
    fn close_nonexistent_name_is_noop() {
        // Should not panic
        close(Some("_nonexistent_close_")).expect("close nonexistent");
    }

    #[test]
    fn close_none_on_empty_registry_is_noop() {
        // Should not panic on empty registry
        close(None).expect("close all on empty registry");
    }

    #[test]
    fn close_twice_is_noop() {
        close(Some("_closed_once_")).expect("first close");
        close(Some("_closed_once_")).expect("second close on already-removed name");
    }

    #[test]
    fn init_rejects_invalid_url_scheme() {
        // URL validation happens before any network I/O — these run without Redis.
        for (url, desc) in [
            ("http://example.com", "http scheme"),
            ("unix:///var/run/redis.sock", "unix scheme"),
            ("", "empty url"),
            ("redis::/bogus", "malformed url"),
        ] {
            let err = init("t", url, None).expect_err(desc);
            assert!(
                err.to_string().contains("invalid redis url"),
                "{desc}: expected 'invalid redis url', got: {err}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Typed blocking API tests (require Redis + WP_REDIS_URL)
    // -----------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn typed_bf_exists_returns_bool() {
        let url = match redis_url() {
            Some(u) => u,
            None => return,
        };
        let name = test_name();
        init(&name, &url, None).expect("init");
        // Non-existent key → false
        let ok = bf_exists(&name, "_wpk_typed_bf_nonexistent", "item1").expect("bf_exists");
        assert!(!ok);
        close(Some(&name)).expect("close");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn typed_hget_returns_option() {
        let url = match redis_url() {
            Some(u) => u,
            None => return,
        };
        let name = test_name();
        init(&name, &url, None).expect("init");
        // Non-existent hash key → None
        let val = hget(&name, "_wpk_typed_hash_nonexistent", "f1").expect("hget");
        assert!(val.is_none());
        close(Some(&name)).expect("close");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn typed_get_returns_option() {
        let url = match redis_url() {
            Some(u) => u,
            None => return,
        };
        let name = test_name();
        init(&name, &url, None).expect("init");
        // Non-existent key → None
        let val = get(&name, "_wpk_typed_str_nonexistent").expect("get");
        assert!(val.is_none());
        close(Some(&name)).expect("close");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn typed_set_exists_returns_bool() {
        let url = match redis_url() {
            Some(u) => u,
            None => return,
        };
        let name = test_name();
        init(&name, &url, None).expect("init");
        // Non-existent set key → false
        let ok = set_exists(&name, "_wpk_typed_set_nonexistent", "m1").expect("set_exists");
        assert!(!ok);
        close(Some(&name)).expect("close");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn typed_bf_madd_and_exists_roundtrip() {
        let url = match redis_url() {
            Some(u) => u,
            None => return,
        };
        let name = test_name();
        init(&name, &url, None).expect("init");

        // First create a Bloom filter
        bf_reserve(&name, "_wpk_typed_bf_roundtrip", 0.01, 1000).expect("bf_reserve");
        // Add items
        let results = bf_madd(
            &name,
            "_wpk_typed_bf_roundtrip",
            &["a".to_string(), "b".to_string()],
        )
        .expect("bf_madd");
        assert_eq!(results.len(), 2);
        assert!(results[0]); // "a" is new
        assert!(results[1]); // "b" is new
        // Check existence
        assert!(bf_exists(&name, "_wpk_typed_bf_roundtrip", "a").expect("bf_exists a"));
        assert!(bf_exists(&name, "_wpk_typed_bf_roundtrip", "b").expect("bf_exists b"));
        assert!(!bf_exists(&name, "_wpk_typed_bf_roundtrip", "c").expect("bf_exists c"));

        close(Some(&name)).expect("close");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn typed_bf_add_empty_slice_returns_empty_vec() {
        let url = match redis_url() {
            Some(u) => u,
            None => return,
        };
        let name = test_name();
        init(&name, &url, None).expect("init");
        bf_reserve(&name, "_wpk_typed_bf_empty", 0.01, 100).expect("bf_reserve");
        let results = bf_madd(&name, "_wpk_typed_bf_empty", &[]).expect("bf_madd empty");
        assert!(results.is_empty());
        close(Some(&name)).expect("close");
    }

    // -----------------------------------------------------------------------
    // Typed async API tests (require Redis + WP_REDIS_URL)
    // -----------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn typed_async_get_returns_option() {
        let url = match redis_url() {
            Some(u) => u,
            None => return,
        };
        let name = test_name();
        init(&name, &url, None).expect("init");
        let val = get_async(&name, "_wpk_typed_async_nonexistent")
            .await
            .expect("get_async");
        assert!(val.is_none());
        close(Some(&name)).expect("close");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn typed_async_bf_exists_hits_pool() {
        let url = match redis_url() {
            Some(u) => u,
            None => return,
        };
        let name = test_name();
        init(&name, &url, None).expect("init");
        let ok = bf_exists_async(&name, "_wpk_typed_async_bf", "x")
            .await
            .expect("bf_exists_async");
        assert!(!ok);
        close(Some(&name)).expect("close");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn typed_async_hget_hits_pool() {
        let url = match redis_url() {
            Some(u) => u,
            None => return,
        };
        let name = test_name();
        init(&name, &url, None).expect("init");
        let val = hget_async(&name, "_wpk_typed_async_h", "f")
            .await
            .expect("hget_async");
        assert!(val.is_none());
        close(Some(&name)).expect("close");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn typed_async_set_exists_hits_pool() {
        let url = match redis_url() {
            Some(u) => u,
            None => return,
        };
        let name = test_name();
        init(&name, &url, None).expect("init");
        let ok = set_exists_async(&name, "_wpk_typed_async_s", "m")
            .await
            .expect("set_exists_async");
        assert!(!ok);
        close(Some(&name)).expect("close");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn typed_async_bf_madd_and_reserve() {
        let url = match redis_url() {
            Some(u) => u,
            None => return,
        };
        let name = test_name();
        init(&name, &url, None).expect("init");
        bf_reserve_async(&name, "_wpk_async_full", 0.01, 100)
            .await
            .expect("bf_reserve_async");
        let results = bf_madd_async(
            &name,
            "_wpk_async_full",
            &["x".to_string(), "y".to_string()],
        )
        .await
        .expect("bf_madd_async");
        assert_eq!(results.len(), 2);
        assert!(results[0]);
        assert!(results[1]);
        close(Some(&name)).expect("close");
    }

    // -----------------------------------------------------------------------
    // CI-safe tests for typed functions (no Redis needed)
    // -----------------------------------------------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn typed_get_nonexistent_provider_returns_error() {
        let err = get_async("_nonexistent_typed_", "any_key")
            .await
            .expect_err("should fail");
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn typed_bf_exists_nonexistent_provider_returns_error() {
        let err = bf_exists_async("_nonexistent_typed_", "any_key", "item")
            .await
            .expect_err("should fail");
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn typed_hget_nonexistent_provider_returns_error() {
        let err = hget_async("_nonexistent_typed_", "any_key", "f")
            .await
            .expect_err("should fail");
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn typed_set_exists_nonexistent_provider_returns_error() {
        let err = set_exists_async("_nonexistent_typed_", "any_key", "m")
            .await
            .expect_err("should fail");
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn typed_bf_madd_nonexistent_provider_returns_error() {
        let err = bf_madd_async("_nonexistent_typed_", "any_key", &["item".to_string()])
            .await
            .expect_err("should fail");
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn typed_bf_reserve_nonexistent_provider_returns_error() {
        let err = bf_reserve_async("_nonexistent_typed_", "any_key", 0.01, 100)
            .await
            .expect_err("should fail");
        assert!(err.to_string().contains("not found"));
    }

    // exec_async BF.RESERVE with non-numeric args tests that the silent
    // fallback (unwrap_or(0.01)) has been replaced with a proper error.
    #[tokio::test(flavor = "multi_thread")]
    async fn bf_reserve_non_numeric_args_via_exec_async_fails() {
        let url = match redis_url() {
            Some(u) => u,
            None => return,
        };
        let name = test_name();
        init(&name, &url, None).expect("init");
        let err = exec_async(
            &name,
            "BF.RESERVE",
            "k",
            &["not_a_number".to_string(), "1000".to_string()],
        )
        .await
        .expect_err("BF.RESERVE with non-numeric error_rate should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("invalid error_rate") || msg.contains("BF.RESERVE"),
            "expected parse error, got: {msg}"
        );
        close(Some(&name)).expect("close");
    }

    /// Redis integration tests are opt-in. Set `WP_REDIS_URL` to enable:
    ///   WP_REDIS_URL=redis://127.0.0.1:6379 cargo test -p wp-knowledge -- redis
    #[test]
    fn redis_tests_require_env_var() {
        // This test exists only to document the opt-in mechanism.
        // It always succeeds; the actual Redis tests are gated by `redis_url()`.
    }
}
