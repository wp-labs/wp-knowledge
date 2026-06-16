# Redis Provider 使用文档

Redis Provider 是 wp-knowledge 的 Redis 外部数据源模块，为 wfusion 的 `external()` 函数提供高速查表后端。

## 适用场景

| 场景 | 命令 | 数据规模 |
|------|------|:--------:|
| 弱口令库 Bloom filter 判定 | `BF.EXISTS` | 10 亿级 |
| 威胁情报 IP 查表 | `HGET` | 百万级 |
| 白名单排除 | `SISMEMBER` | 十万级 |
| 标签查询 | `GET` | — |

## 配置

在 `knowdb.toml` 中配置 Redis 连接，wp-knowledge 加载时自动完成初始化：

```toml
version = 2

[provider.redis]
connection_uri = "redis://127.0.0.1:6379"
pool_size = 8                     # 可选，ConnectionManager 内部管理多路复用
connect_timeout_ms = 3000         # 可选，默认 3000（3s）
command_timeout_ms = 100          # 可选，默认 100（100ms）
```

### 与 SQL 数据库共存

```toml
version = 2

[provider.sqldb]
kind = "postgres"
connection_uri = "postgres://user:pass@127.0.0.1/db"

[provider.redis]
connection_uri = "redis://127.0.0.1:6379"
command_timeout_ms = 200
```

## API 参考

通过 `[fun]` 配置段将 Redis 命令封装为命名查询，wfusion 只需传 service name + arg，不感知底层数据结构。

### 配置

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
# 无 key，arg = "user:123" → GET user:123
```

| 字段 | 必填 | 默认 | 说明 |
|------|:---:|:---:|------|
| `call` | ✅ | — | `bf_exists` / `sismember` / `hget` / `get` |
| `key` | ❌ | — | Redis key。`get` 时可省略，arg 作为 key |
| `cache` | ❌ | `true` | `false` 时该 service 不走缓存 |
| `ttl_ms` | ❌ | 无 | 缓存过期时间（毫秒）。不设时仅用 generation 失效 |

### 调用

```rust
use wp_knowledge::facade::{external_exists, external_value};

// call = "bf_exists" | "sismember" → bool
let ok: bool = external_exists("password_check", "abc123")?;

// call = "hget" | "get" → Option<String>
let label: Option<String> = external_value("threat_actor", "1.2.3.4")?;
```

| 函数 | 签名 | 返回值 |
|------|------|:------:|
| `external_exists` | `(service, arg)` | `bool` |
| `external_value` | `(service, arg)` | `Option<String>` |

## 结果缓存

`external_exists` 和 `external_value` 底层复用 Redis 读函数的 LRU 缓存。

### 全局配置

SQL 和 Redis 共用 `[cache]`：

```toml
[cache]
enabled = true        # 全局开关（SQL + Redis）
capacity = 1024       # LRU 容量
ttl_ms = 30000        # TTL（毫秒）
```

### 按 service 控制

在 `[fun.<name>]` 中覆盖：

```toml
[fun.password_check]
call = "bf_exists"
key = "weak_passwords"
cache = false          # 关闭该 service 的缓存

[fun.threat_actor]
call = "hget"
key = "threat_actors"
ttl_ms = 60000         # 该 service 独立 TTL
```

`[fun.<name>]` 中未配置时使用 `[cache]` 全局默认。

## 超时控制

| 超时类型 | 默认值 | 配置字段 |
|----------|:------:|----------|
| 命令超时 | 100ms | `command_timeout_ms` |
| 连接超时 | 3000ms | `connect_timeout_ms` |

命令超时通过 `tokio::time::timeout` 在每次命令执行时生效。

## 错误处理

| 场景 | 错误信息示例 |
|------|-------------|
| Service 未定义 | `external service 'xxx' not found` |
| Call 类型不匹配 | `external service 'xxx' returns value, not bool` |
| 未注册 [fun] | `external: no [fun] definitions registered` |
| 连接失败 | `redis connect failed for 'redis://...'` |
| 命令超时 | `redis command 'HGET' on 'xxx' timed out after 100ms` |

## 测试

### 运行测试

Redis 测试需要本地 Redis 实例（含 RedisBloom 模块）。通过环境变量 `WP_REDIS_URL` 启用：

```bash
WP_REDIS_URL=redis://127.0.0.1:6379 cargo test -p wp-knowledge -- redis
```

### CI 安全测试

**fun 注册表测试：**

| 测试 | 验证内容 |
|------|---------|
| `resolve_service_not_found` | 未定义 service 报错 |
| `resolve_type_mismatch` | call 类型不匹配报错 |
| `resolve_bool_services` | bf_exists / sismember 解析 |
| `resolve_value_services` | hget / get 解析 |
| `no_key_uses_arg` | 无 key 时 arg 作为 key |

**缓存单元测试（无需 Redis）：**

| 测试 | 验证内容 |
|------|---------|
| `redis_cache_hit_and_miss` | 正常存取 |
| `redis_cache_global_enabled_access` | 全局开关启用 |
| `redis_cache_global_disabled_blocks_all` | 全局关闭阻止所有读写 |
| `redis_cache_generation_isolation` | generation 隔离 |
| `redis_cache_ttl_expiry` | TTL 过期后缓存失效 |
| `redis_cache_no_ttl_never_expires` | ttl=0 永不过期 |

## 与 wfusion 集成

```
wf-runtime bootstrap
  │
  └── knowdb.toml (provider.redis) → 自动初始化

wf-engine eval
  │
  └── external("password_check", hash)
        └── wp_knowledge::facade::external_exists("password_check", hash)
             → bool
```

wfusion 不直接依赖 `redis` crate，全部通过 `wp_knowledge::facade` 访问。

## 架构设计

```
┌─────────────────────────────────────────┐
│              facade (公共 API)           │
│  external_exists / external_value       │
└──────────────────┬──────────────────────┘
                   │
┌──────────────────▼──────────────────────┐
│           fun.rs (命名查询注册表)        │
│  FUN_MAP: resolve → bf_exists / hget   │
└──────────────────┬──────────────────────┘
                   │
┌──────────────────▼──────────────────────┐
│           redis.rs (内部实现)            │
│  RedisRegistry / ConnectionManager      │
└──────────────────┬──────────────────────┘
                   │
┌──────────────────▼──────────────────────┐
│         redis crate (0.25)              │
│    ConnectionManager (多路复用)          │
└─────────────────────────────────────────┘
```

## 版本历史

| 版本 | 变更 |
|------|------|
| 0.14.1 | 新增 `[fun]` 命名查询、`external_exists` / `external_value`、per-service 缓存 + TTL |
| 0.14.0 | 首次发布：连接池去重、双层超时、结果缓存 |
