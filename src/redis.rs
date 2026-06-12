use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::error::{KnowReason, KnowledgeResult};
use orion_error::conversion::ToStructError;
use redis::aio::ConnectionManager;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 100;

/// Supported Redis commands (uppercase).
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
// Public (crate-internal) API
// ---------------------------------------------------------------------------

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

    // If a pool for this URL already exists, reuse it
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
    reg.register(name, url, pool)
}

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

    fn redis_err(cmd: &str, name: &str, err: redis::RedisError) -> crate::error::KnowledgeError {
        KnowReason::from_logic()
            .to_err()
            .with_detail(format!("redis command '{cmd}' on '{name}' failed: {err}"))
    }

    let (timeout, mut conn) = {
        let pool = registry()
            .lock()
            .expect("redis registry lock poisoned")
            .resolve(name)?;
        let timeout = pool.timeout();
        // Clone ConnectionManager before dropping the registry lock (lock is
        // never held across an await point).
        let conn = pool.conn.clone();
        (timeout, conn)
    };

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
                Ok(results
                    .iter()
                    .map(|b| if *b { "1" } else { "0" })
                    .collect::<Vec<_>>()
                    .join(","))
            }
            "BF.RESERVE" => {
                let error_rate: f64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(0.01);
                let capacity: i64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1000);
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
        Ok(Err(e)) => Err(redis_err(cmd, name, e)),
        Err(_elapsed) => Err(KnowReason::from_logic().to_err().with_detail(format!(
            "redis command '{cmd}' on '{name}' timed out after {}ms",
            timeout.as_millis()
        ))),
    }
}

pub(crate) fn exec_blocking(
    name: &str,
    cmd: &str,
    key: &str,
    args: &[String],
) -> KnowledgeResult<String> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(exec_async(name, cmd, key, args))
    })
}

pub(crate) fn ping_blocking(name: &str) -> KnowledgeResult<bool> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let (timeout, mut conn) = {
                let pool = registry()
                    .lock()
                    .expect("redis registry lock poisoned")
                    .resolve(name)?;
                let timeout = pool.timeout();
                let conn = pool.conn.clone();
                (timeout, conn)
            };

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
    })
}

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

    /// Redis integration tests are opt-in. Set `WP_REDIS_URL` to enable:
    ///   WP_REDIS_URL=redis://127.0.0.1:6379 cargo test -p wp-knowledge -- redis
    #[test]
    fn redis_tests_require_env_var() {
        // This test exists only to document the opt-in mechanism.
        // It always succeeds; the actual Redis tests are gated by `redis_url()`.
    }
}
