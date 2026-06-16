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

The `[fun]` section wraps Redis commands as named queries — wfusion only passes service name + arg, without knowing the underlying data structure.

### Configuration

```toml
[fun.password_check]
call = "bf_exists"
key = "weak_passwords"

[fun.threat_actor]
call = "hget"
key = "threat_actors"

[fun.ip_whitelist]
call = "sismember"
key = "allowed_ips"

[fun.app_config]
call = "get"
key = "app_config"

[fun.user_tag]
call = "get"
# No key — arg "user:123" → GET user:123
```

| Field | Required | Default | Description |
|-------|:---:|:---:|------|
| `call` | Yes | — | `bf_exists` / `sismember` / `hget` / `get` |
| `key` | No | — | Redis key. Can be omitted when `call = "get"`, then arg becomes the key |
| `cache` | No | `true` | `false` bypasses cache for this service |
| `ttl_ms` | No | none | Cache expiry in milliseconds. When unset, uses generation-based eviction only |

### Invocation

```rust
use wp_knowledge::facade::{external_exists, external_value};

// call = "bf_exists" | "sismember" → bool
let ok: bool = external_exists("password_check", "abc123")?;

// call = "hget" | "get" → Option<String>
let label: Option<String> = external_value("threat_actor", "1.2.3.4")?;
```

| Function | Signature | Returns |
|----------|-----------|:------:|
| `external_exists` | `(service, arg)` | `bool` |
| `external_value` | `(service, arg)` | `Option<String>` |

## Result Cache

`external_exists` and `external_value` reuse the LRU cache built into the underlying Redis read functions.

### Global Config

SQL and Redis share the same `[cache]`:

```toml
[cache]
enabled = true        # global toggle (SQL + Redis)
capacity = 1024       # LRU capacity
ttl_ms = 30000        # TTL (milliseconds)
```

### Per Service

Override in `[fun.<name>]`:

```toml
[fun.password_check]
call = "bf_exists"
key = "weak_passwords"
cache = false          # disable cache for this service

[fun.threat_actor]
call = "hget"
key = "threat_actors"
ttl_ms = 60000         # custom TTL for this service
```

When not set in `[fun.<name>]`, falls back to `[cache]` defaults.

## Timeout Control

| Timeout Type | Default | Config Field |
|-------------|:-------:|--------------|
| Command timeout | 100ms | `command_timeout_ms` |
| Connection timeout | 3000ms | `connect_timeout_ms` |

Command timeout is enforced via `tokio::time::timeout` on each execution.

## Error Handling

| Scenario | Error Message Example |
|----------|----------------------|
| Service not found | `external service 'xxx' not found` |
| Type mismatch | `external service 'xxx' returns value, not bool` |
| No [fun] registered | `external: no [fun] definitions registered` |
| Connection failure | `redis connect failed for 'redis://...'` |
| Command timeout | `redis command 'HGET' on 'xxx' timed out after 100ms` |

## Testing

### Running Tests

Redis tests require a local Redis instance (with RedisBloom module). Enable via `WP_REDIS_URL`:

```bash
WP_REDIS_URL=redis://127.0.0.1:6379 cargo test -p wp-knowledge -- redis
```

### CI-Safe Tests

**fun registry tests:**

| Test | What It Verifies |
|------|-----------------|
| `resolve_service_not_found` | Undefined service error |
| `resolve_type_mismatch` | Call type mismatch error |
| `resolve_bool_services` | bf_exists / sismember resolution |
| `resolve_value_services` | hget / get resolution |
| `no_key_uses_arg` | Missing key falls back to arg |

**Cache unit tests (no Redis needed):**

| Test | What It Verifies |
|------|-----------------|
| `redis_cache_hit_and_miss` | Normal get/put |
| `redis_cache_global_enabled_access` | Global enable |
| `redis_cache_global_disabled_blocks_all` | Global disable blocks all reads/writes |
| `redis_cache_generation_isolation` | Generation isolation |
| `redis_cache_ttl_expiry` | TTL expiry evicts cache |
| `redis_cache_no_ttl_never_expires` | Zero TTL means no expiry |

## wfusion Integration

```
wf-runtime bootstrap
  │
  └── knowdb.toml (provider.redis) → auto-initialize

wf-engine eval
  │
  └── external("password_check", hash)
        └── wp_knowledge::facade::external_exists("password_check", hash)
             → bool
```

wfusion does not directly depend on the `redis` crate; all access goes through `wp_knowledge::facade`.

## Architecture

```
┌─────────────────────────────────────────┐
│              facade (Public API)         │
│  external_exists / external_value       │
└──────────────────┬──────────────────────┘
                   │
┌──────────────────▼──────────────────────┐
│           fun.rs (Named Query Registry)  │
│  FUN_MAP: resolve → bf_exists / hget   │
└──────────────────┬──────────────────────┘
                   │
┌──────────────────▼──────────────────────┐
│           redis.rs (Internal)            │
│  RedisRegistry / ConnectionManager      │
└──────────────────┬──────────────────────┘
                   │
┌──────────────────▼──────────────────────┐
│         redis crate (0.25)              │
│    ConnectionManager (multiplexing)      │
└─────────────────────────────────────────┘
```

## Version History

| Version | Changes |
|---------|---------|
| 0.14.1 | Added `[fun]` named queries, `external_exists` / `external_value`, per-service cache + TTL |
| 0.14.0 | Initial release: pool dedup, two-layer timeout, result cache |
