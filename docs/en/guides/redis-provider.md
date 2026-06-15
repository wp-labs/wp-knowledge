# Redis Provider Usage Guide

The Redis Provider is wp-knowledge's Redis external data source module, providing a high-speed lookup backend for wfusion's `external()` function.

## Use Cases

| Scenario | Command | Data Scale |
|----------|---------|:----------:|
| Weak password Bloom filter check | `BF.EXISTS` | Billion-level |
| Threat intelligence IP lookup | `HGET` | Million-level |
| Whitelist exclusion | `SISMEMBER` | Hundred-thousand-level |
| Tag query | `GET` | — |

## Configuration

Configure Redis in `knowdb.toml` — wp-knowledge auto-initializes the connection on load:

```toml
version = 2

[provider.redis]
connection_uri = "redis://127.0.0.1:6379"
pool_size = 8                     # Optional; ConnectionManager handles multiplexing
connect_timeout_ms = 3000         # Optional, default 3000 (3s)
command_timeout_ms = 100          # Optional, default 100 (100ms)
```

### Coexisting with SQL Databases

```toml
version = 2

[provider.sqldb]
kind = "postgres"
connection_uri = "postgres://user:pass@127.0.0.1/db"

[provider.redis]
connection_uri = "redis://127.0.0.1:6379"
command_timeout_ms = 200
```

## API Reference

```rust
use wp_knowledge::facade::{redis_bf_exists, redis_hget, redis_get, redis_set_exists};

// Bloom filter existence check
let exists: bool = redis_bf_exists("weak_passwords", "hash_value")?;

// Hash field query
let label: Option<String> = redis_hget("ip:1.2.3.4", "label")?;
match label {
    Some(v) => { /* hit */ }
    None => { /* miss */ }
}

// Simple KV query
let tag: Option<String> = redis_get("user:123")?;

// Set membership check
let ok: bool = redis_set_exists("allowed_ips", "10.0.0.1")?;
```

| Function | Signature | Returns |
|----------|-----------|:------:|
| `redis_bf_exists` | `(key, item)` | `bool` |
| `redis_hget` | `(key, field)` | `Option<String>` |
| `redis_get` | `(key)` | `Option<String>` |
| `redis_set_exists` | `(key, member)` | `bool` |
| `redis_bf_add` | `(key, items)` | `Vec<bool>` |
| `redis_bf_create` | `(key, error_rate, capacity)` | `()` |

## Supported Commands

| Function | Underlying Command | Purpose | Returns |
|----------|-------------------|---------|:------:|
| `redis_bf_exists` | `BF.EXISTS` | Bloom filter existence check | `bool` |
| `redis_hget` | `HGET` | Hash field query | `Option<String>` |
| `redis_get` | `GET` | Simple KV query | `Option<String>` |
| `redis_set_exists` | `SISMEMBER` | Set membership check | `bool` |
| `redis_bf_add` | `BF.MADD` | Bloom filter batch add | `Vec<bool>` |
| `redis_bf_create` | `BF.RESERVE` | Bloom filter creation | `()` |

## Result Cache

The four read functions (`redis_bf_exists`, `redis_hget`, `redis_get`, `redis_set_exists`) automatically use an in-process LRU cache to reduce Redis round-trips.

SQL and Redis share the same `[cache]` configuration:

```toml
[cache]                # shared by SQL + Redis
enabled = true
capacity = 1024
ttl_ms = 30000

[[cache.redis_key]]    # disable cache for specific keys
key = "volatile_tags"
enabled = false
```

> Keys not listed in `[[cache.redis_key]]` use `[cache].enabled`. No config needed for keys that don't need special treatment.

| Feature | Description |
|---------|-------------|
| Cache Key | `(generation, cmd_tag, key_hash, args_hash)` |
| Scope | Four read functions |
| Invalidation | Provider reload increments generation, old entries naturally expire |
| Per-key control | `[[cache.redis_key]]` overrides; checked in both `redis_cache_get` and `redis_cache_put` |
| Write bypass | `redis_bf_add`, `redis_bf_create` write directly to Redis |

## Timeout Control

Two-layer timeout mechanism:

| Timeout Type | Default | Config Field |
|-------------|:-------:|--------------|
| Command timeout | 100ms | `command_timeout_ms` |
| Connection timeout | 3000ms | `connect_timeout_ms` |

Command timeout is enforced via `tokio::time::timeout` on each execution.

## Error Handling

| Scenario | Error Message Example |
|----------|----------------------|
| Provider not found | `redis provider 'xxx' not found` |
| Invalid URL | `invalid redis url: 'http://...'` |
| Connection failure | `redis connect failed for 'redis://...'` |
| Command timeout | `redis command 'HGET' on 'xxx' timed out after 100ms` |
| Command execution failure | `redis command 'GET' on 'xxx' failed: ...` |

## Dependencies

```toml
[dependencies]
redis = { version = "0.25", features = ["tokio-comp", "connection-manager"] }
```

## Testing

### Running Tests

Redis tests require a local Redis instance (with RedisBloom module). Enable via `WP_REDIS_URL`:

```bash
WP_REDIS_URL=redis://127.0.0.1:6379 cargo test -p wp-knowledge -- redis
```

### CI-Safe Tests

**Redis integration tests:**

| Test | What It Verifies |
|------|-----------------|
| `typed_bf_exists_returns_bool` | Typed BF.EXISTS |
| `typed_hget_returns_option` | Typed HGET |
| `typed_get_returns_option` | Typed GET |
| `typed_set_exists_returns_bool` | Typed SISMEMBER |
| `typed_bf_madd_and_exists_roundtrip` | reserve → madd → exists round-trip |
| `typed_bf_add_empty_slice_returns_empty_vec` | Empty slice returns `[]` |
| `typed_async_*` series | Async API verification |
| `bf_reserve_non_numeric_args_via_exec_async_fails` | Non-numeric args return error |

**Cache unit tests (no Redis needed):**

| Test | What It Verifies |
|------|-----------------|
| `redis_cache_hit_and_miss` | Normal get/put |
| `redis_cache_disabled_key_is_not_read` | Disabled key returns None |
| `redis_cache_disabled_key_is_not_stored` | Disabled key is not written |
| `redis_cache_per_key_override_works_independently` | Per-key isolation |
| `redis_cache_global_disabled_blocks_all` | Global disable |
| `redis_cache_generation_isolation` | Generation isolation |

## wfusion Integration

```
wf-runtime bootstrap
  │
  └── knowdb.toml (provider.redis) → auto-initialize

wf-engine eval
  │
  └── ExternalCall { service, args }
        └── wp_knowledge::facade::redis_bf_exists / redis_hget / ...
             → wfusion side uses Rust native types directly
```

wfusion does not directly depend on the `redis` crate; all access goes through `wp_knowledge::facade`.

## Architecture

```
┌──────────────────────────────────────────────────┐
│              facade (Public API)                  │
│  redis_bf_exists / redis_hget / redis_get / ...  │
│  ├─ cache check (redis_cache_get / redis_cache_put) │
│  └─ call redis.rs typed functions                 │
└───────────┬──────────────────────────┬───────────┘
            │                          │
    ┌───────▼───────┐          ┌───────▼───────────┐
    │  runtime.rs   │          │   redis.rs         │
    │  RedisCache    │          │  RedisRegistry     │
    │  ├─ get/put    │          │  ├─ names          │
    │  ├─ per-key    │          │  ├─ pools          │
    │  ├─ generation │          │  └─ resolve_pool   │
    │  └─ LruCache   │          └─────────┬──────────┘
    └───────────────┘                     │
                              ┌──────────▼──────────┐
                              │  redis crate (0.25)  │
                              │  ConnectionManager    │
                              └──────────────────────┘
```

## Version History

| Version | Changes |
|---------|---------|
| 0.14.0 | Initial release: 6 typed APIs, pool dedup, two-layer timeout, result cache, per-key control |
