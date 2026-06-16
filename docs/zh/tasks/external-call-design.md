# External Call 设计

中文 | [English](../../en/tasks/external-call-design.md)

## 背景

wf-engine 新增 `external("service_name", arg)` WFL 函数，允许规则在运行时调用外部服务做点查询（弱口令检查、威胁情报 IP 查询等）。P0 仅支持 Redis 后端。

wp-knowledge 需要提供"命名查询"抽象，让上层只需传 service name + arg，不感知底层是 Bloom、Hash 还是 Set。

## 配置

```toml
[fun.<name>]
call = "bf_exists" | "sismember" | "hget" | "get"  # 必填
key = "<redis_key>"                                  # get 可省略，其余必填
cache = true                                         # 可选，默认 true
ttl_ms = 60000                                       # 可选，默认只 generation 失效
```

| 字段 | 必填 | 说明 |
|------|:---:|------|
| `call` | ✅ | 底层 Redis 命令 |
| `key` | ❌ | Redis key。`call = "get"` 时可省略，arg 作为 key |
| `cache` | ❌ | 默认 `true`；设为 `false` 不缓存 |
| `ttl_ms` | ❌ | 默认只用 generation 失效；设为 >0 叠加时间过期（毫秒） |

| call | 返回值 | API |
|------|:---:|------|
| `bf_exists` | `bool` | `external_exists` |
| `sismember` | `bool` | `external_exists` |
| `hget` | `Option<String>` | `external_value` |
| `get` | `Option<String>` | `external_value` |

**配置示例**：

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
# 无 key，arg = "user:123" → GET user:123
```

## API

```rust
/// 执行 [fun.<name>] 中 call = "bf_exists" | "sismember" 的命名查询。
pub fn external_exists(service: &str, arg: &str) -> KnowledgeResult<bool>

/// 执行 [fun.<name>] 中 call = "hget" | "get" 的命名查询。
pub fn external_value(service: &str, arg: &str) -> KnowledgeResult<Option<String>>
```

**调用示例**：

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

## 调用链路

```
WFL: external("password_check", e.password_hash)
  → wf-engine eval.rs
    → wf-runtime ExternalRuntime
      → wp_knowledge::facade::external_exists("password_check", "<hash>")
        ├─ 查找全局注册表: FUN_MAP
        ├─ 校验 call 类型匹配
        ├─ redis_bf_exists("weak_passwords", "<hash>")
        └─ 返回 bool
```

## 缓存

待实现。当前直接调用 Redis，缓存将复用 `redis_bf_exists` / `redis_hget` 等函数内置的 LRU 缓存。

## 约束

| 规则 | 说明 |
|------|------|
| name 唯一 | `[fun]` 是 HashMap，TOML 自动去重 |
| call 类型匹配 | `bf_exists`/`sismember` 只能调 `external_exists`；`hget`/`get` 只能调 `external_value` |
| Redis 依赖 | `[fun]` 需要 `[provider.redis]` |

## 错误

| 场景 | 返回值 |
|------|------|
| service 未定义 | `Err("external service 'xxx' not found")` |
| call 类型不匹配 | `Err("external service 'xxx' returns value, not bool")` |
| 未注册 [fun] | `Err("external: no [fun] definitions registered")` |
| Redis 连接失败 | 透传 Redis 层错误 |

## 架构

```
┌─────────────────────────────────────────┐
│              facade (公共 API)           │
│  external_exists / external_value       │
└──────────────────┬──────────────────────┘
                   │
┌──────────────────▼──────────────────────┐
│           fun.rs (全局注册表)            │
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

## 关联

- wfusion 侧: wp-reactor `external()` 实现
- 设计文档: wp-reactor/docs/design/external-function-design.md
- Redis provider: docs/zh/guides/redis-provider.md
- GitHub Issue: [#22](https://github.com/wp-labs/wp-knowledge/issues/22)
