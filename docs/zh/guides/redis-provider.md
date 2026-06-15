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

配置完成后，通过以下 API 访问 Redis 数据：

```rust
use wp_knowledge::facade::{redis_bf_exists, redis_hget, redis_get, redis_set_exists};

// Bloom filter 存在性检查
let exists: bool = redis_bf_exists("weak_passwords", "hash_value")?;

// Hash 字段查询
let label: Option<String> = redis_hget("ip:1.2.3.4", "label")?;
match label {
    Some(v) => { /* 命中 */ }
    None => { /* 未命中 */ }
}

// 简单 KV 查询
let tag: Option<String> = redis_get("user:123")?;

// Set 成员判定
let ok: bool = redis_set_exists("allowed_ips", "10.0.0.1")?;
```

| 函数 | 签名 | 返回值 |
|------|------|:------:|
| `redis_bf_exists` | `(key, item)` | `bool` |
| `redis_hget` | `(key, field)` | `Option<String>` |
| `redis_get` | `(key)` | `Option<String>` |
| `redis_set_exists` | `(key, member)` | `bool` |
| `redis_bf_add` | `(key, items)` | `Vec<bool>` |
| `redis_bf_create` | `(key, error_rate, capacity)` | `()` |

## 支持的命令

| 函数 | 底层命令 | 用途 | 返回值 |
|------|----------|------|:------:|
| `redis_bf_exists` | `BF.EXISTS` | Bloom filter 存在性检查 | `bool` |
| `redis_hget` | `HGET` | Hash 字段查询 | `Option<String>` |
| `redis_get` | `GET` | 简单 KV 查询 | `Option<String>` |
| `redis_set_exists` | `SISMEMBER` | Set 成员判定 | `bool` |
| `redis_bf_add` | `BF.MADD` | Bloom filter 批量添加 | `Vec<bool>` |
| `redis_bf_create` | `BF.RESERVE` | Bloom filter 创建 | `()` |

## 结果缓存

四个读函数（`redis_bf_exists`、`redis_hget`、`redis_get`、`redis_set_exists`）自动使用进程内 LRU 缓存，减少 Redis 往返。

SQL 和 Redis 共用 `[cache]` 配置：

```toml
[cache]                # SQL + Redis 共用
enabled = true
capacity = 1024
ttl_ms = 30000

[[cache.redis_key]]    # 按 key 关闭缓存
key = "volatile_tags"
enabled = false
```

> `[[cache.redis_key]]` 中未列出的 key 使用 `[cache].enabled`。不需要单独关闭的 key 不用配置。

| 特性 | 说明 |
|------|------|
| 缓存 Key | `(generation, cmd_tag, key_hash, args_hash)` |
| 生效范围 | 四个读函数 |
| 失效机制 | provider reload 时 generation 递增，旧 key 自然淘汰 |
| 按 key 开关 | `[[cache.redis_key]]` 覆盖，`redis_cache_get` / `redis_cache_put` 均检查 |
| 写函数不缓存 | `redis_bf_add`、`redis_bf_create` 直接写 Redis |

## 超时控制

双层超时机制：

| 超时类型 | 默认值 | 配置字段 |
|----------|:------:|----------|
| 命令超时 | 100ms | `command_timeout_ms` |
| 连接超时 | 3000ms | `connect_timeout_ms` |

命令超时通过 `tokio::time::timeout` 在每次命令执行时生效。

## 错误处理

| 场景 | 错误信息示例 |
|------|-------------|
| Provider 未找到 | `redis provider 'xxx' not found` |
| 无效 URL | `invalid redis url: 'http://...'` |
| 连接失败 | `redis connect failed for 'redis://...'` |
| 命令超时 | `redis command 'HGET' on 'xxx' timed out after 100ms` |
| 命令执行失败 | `redis command 'GET' on 'xxx' failed: ...` |

## 依赖

```toml
[dependencies]
redis = { version = "0.25", features = ["tokio-comp", "connection-manager"] }
```

## 测试

### 运行测试

Redis 测试需要本地 Redis 实例（含 RedisBloom 模块）。通过环境变量 `WP_REDIS_URL` 启用：

```bash
WP_REDIS_URL=redis://127.0.0.1:6379 cargo test -p wp-knowledge -- redis
```

### CI 安全测试

**Redis 集成测试：**

| 测试 | 验证内容 |
|------|---------|
| `typed_bf_exists_returns_bool` | 类型化 BF.EXISTS |
| `typed_hget_returns_option` | 类型化 HGET |
| `typed_get_returns_option` | 类型化 GET |
| `typed_set_exists_returns_bool` | 类型化 SISMEMBER |
| `typed_bf_madd_and_exists_roundtrip` | reserve → madd → exists 全链路 |
| `typed_bf_add_empty_slice_returns_empty_vec` | 空 slice 返回 `[]` |
| `typed_async_*` 系列 | 异步 API 验证 |
| `bf_reserve_non_numeric_args_via_exec_async_fails` | 非数值参数报错 |

**缓存单元测试（无需 Redis）：**

| 测试 | 验证内容 |
|------|---------|
| `redis_cache_hit_and_miss` | 正常存取 |
| `redis_cache_disabled_key_is_not_read` | disabled key 不返回缓存 |
| `redis_cache_disabled_key_is_not_stored` | disabled key 不写入缓存 |
| `redis_cache_per_key_override_works_independently` | 按 key 控制互不影响 |
| `redis_cache_global_disabled_blocks_all` | 全局关闭 |
| `redis_cache_generation_isolation` | generation 隔离 |

## 与 wfusion 集成

```
wf-runtime bootstrap
  │
  └── knowdb.toml (provider.redis) → 自动初始化

wf-engine eval
  │
  └── ExternalCall { service, args }
        └── wp_knowledge::facade::redis_bf_exists / redis_hget / ...
             → wfusion 侧直接使用 Rust 原生类型
```

wfusion 不直接依赖 `redis` crate，全部通过 `wp_knowledge::facade` 访问。

## 架构设计

```
┌──────────────────────────────────────────────────┐
│              facade (公共 API)                    │
│  redis_bf_exists / redis_hget / redis_get / ...  │
│  ├─ 缓存检查 (redis_cache_get / redis_cache_put) │
│  └─ 调用 redis.rs 类型化函数                      │
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

## 版本历史

| 版本 | 变更 |
|------|------|
| 0.14.0 | 首次发布：6 种类型化 API、连接池去重、双层超时、结果缓存、per-key 缓存控制 |
