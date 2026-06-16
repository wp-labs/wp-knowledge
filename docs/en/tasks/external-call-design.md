# External Call Design

English | [中文](../../zh/tasks/external-call-design.md)

## Background

wf-engine adds `external("service_name", arg)` WFL function, allowing rules to call external services at runtime for point queries (password check, threat intelligence IP lookup, etc.). P0 only supports Redis backend.

wp-knowledge needs a "named query" abstraction layer — callers only pass service name + arg, without knowing the underlying data structure (Bloom, Hash, Set, or String).

## Configuration

```toml
[fun.<name>]
call = "bf_exists" | "sismember" | "hget" | "get"  # Required
key = "<redis_key>"                                  # Optional for "get"; required otherwise
cache = true                                         # Optional, default true
ttl_ms = 60000                                       # Optional, default: generation-based eviction only
```

| Field | Required | Description |
|-------|:---:|------|
| `call` | Yes | Underlying Redis command |
| `key` | No | Redis key. Can be omitted when `call = "get"`, then arg becomes the key |
| `cache` | No | Default `true`; set `false` to disable caching |
| `ttl_ms` | No | Default generation-based eviction; set >0 for additional time-based expiry (ms) |

| call | Returns | API |
|------|:---:|------|
| `bf_exists` | `bool` | `external_exists` |
| `sismember` | `bool` | `external_exists` |
| `hget` | `Option<String>` | `external_value` |
| `get` | `Option<String>` | `external_value` |

**Example**:

```toml
version = 2

[provider.redis]
connection_uri = "redis://127.0.0.1:6379"

[fun.password_check]
call = "bf_exists"
key = "weak_passwords"

[fun.ip_whitelist]
call = "sismember"
key = "allowed_ips"

[fun.threat_actor]
call = "hget"
key = "threat_actors"

[fun.app_config]
call = "get"
key = "app_config"

[fun.user_tag]
call = "get"
# No key — arg "user:123" → GET user:123
```

## API

```rust
/// Execute a [fun.<name>] query with call = "bf_exists" | "sismember".
pub fn external_exists(service: &str, arg: &str) -> KnowledgeResult<bool>

/// Execute a [fun.<name>] query with call = "hget" | "get".
pub fn external_value(service: &str, arg: &str) -> KnowledgeResult<Option<String>>
```

**Examples**:

```rust
// BF.EXISTS weak_passwords <hash>
external_exists("password_check", "abc123")?  // → true / false

// SISMEMBER allowed_ips <ip>
external_exists("ip_whitelist", "10.0.0.1")?  // → true / false

// HGET threat_actors <ip>
external_value("threat_actor", "1.2.3.4")?    // → Some("apt29") / None

// GET app_config
external_value("app_config", "")?              // → Some("debug") / None

// GET user:123
external_value("user_tag", "user:123")?        // → Some("admin") / None
```

## Call Chain

```
WFL: external("password_check", e.password_hash)
  → wf-engine eval.rs
    → wf-runtime ExternalRuntime
      → wp_knowledge::facade::external_exists("password_check", "<hash>")
        ├─ Lookup global registry: FUN_MAP
        ├─ Validate call type
        ├─ redis_bf_exists("weak_passwords", "<hash>")
        └─ Return bool
```

## Cache

TBD. Currently calls Redis directly. Will reuse the LRU cache built into `redis_bf_exists` / `redis_hget` etc.

## Constraints

| Rule | Description |
|------|-------------|
| Unique name | `[fun]` is a HashMap; TOML enforces unique keys |
| Call type match | `bf_exists`/`sismember` → `external_exists`; `hget`/`get` → `external_value` |
| Redis dependency | `[fun]` requires `[provider.redis]` |

## Errors

| Scenario | Return Value |
|----------|-------------|
| Service not found | `Err("external service 'xxx' not found")` |
| Type mismatch | `Err("external service 'xxx' returns value, not bool")` |
| No [fun] registered | `Err("external: no [fun] definitions registered")` |
| Redis connection failure | Propagated from Redis layer |

## Architecture

```
┌─────────────────────────────────────────┐
│              facade (Public API)         │
│  external_exists / external_value       │
└──────────────────┬──────────────────────┘
                   │
┌──────────────────▼──────────────────────┐
│           fun.rs (Global Registry)       │
│  FUN_MAP: OnceLock<HashMap<name, Spec>> │
│  ├─ resolve(service, returns_bool)      │
│  ├─ external_exists                     │
│  └─ external_value                      │
└──────────────────┬──────────────────────┘
                   │
┌──────────────────▼──────────────────────┐
│           redis.rs                       │
│  bf_exists / set_exists / hget / get    │
└─────────────────────────────────────────┘
```

## Related

- wfusion side: wp-reactor `external()` implementation
- Design doc: wp-reactor/docs/design/external-function-design.md
- Redis provider: docs/en/guides/redis-provider.md
- GitHub Issue: [#22](https://github.com/wp-labs/wp-knowledge/issues/22)
