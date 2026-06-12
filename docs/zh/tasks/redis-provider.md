# Redis Provider 开发需求

中文 | [English](../../en/tasks/redis-provider.md)

## 背景

wp-knowledge 当前仅支持 PostgreSQL 数据访问。wfusion 的 `external()` 函数需要 Redis 作为高速查表后端，用于：

- 弱口令库 Bloom filter 判定（`BF.EXISTS`，10 亿级）
- 威胁情报 IP 查表（`HGET`，百万级）
- 白名单排除（`SISMEMBER`，十万级）
- 标签查询（`GET`）

## 接口需求

### 1. 初始化

```rust
/// 初始化 Redis 连接池
///
/// name: provider 名称
/// url:  redis://127.0.0.1:6379
/// pool_size: 连接池大小，默认 8
pub fn init_redis_provider(
    name: &str,
    url: &str,
    pool_size: Option<usize>,
) -> Result<(), KnowledgeError>;
```

对同一个 `endpoint` 的多次调用共享连接池（按 URL 去重）。

### 2. 命令执行

```rust
/// 执行 Redis 命令并返回结果
///
/// name: provider 名称
/// cmd:  Redis 命令
/// key:  Redis key 名
/// args: 命令参数
pub fn redis_exec(
    name: &str,
    cmd: &str,
    key: &str,
    args: &[&str],
) -> Result<String, KnowledgeError>;
```

### 3. 健康检查

```rust
pub fn redis_ping(name: &str) -> Result<bool, KnowledgeError>;
```

### 4. 关闭

```rust
pub fn redis_close(name: Option<&str>) -> Result<(), KnowledgeError>;
```

## 支持的命令

| 命令 | 用途 | 返回值 | 优先级 |
|------|------|:-----:|:-----:|
| `BF.EXISTS` | Bloom filter 存在性检查 | `"1"` / `"0"` | P0 |
| `HGET` | Hash 字段查询 | 字符串 / `""` | P0 |
| `GET` | 简单 KV 查询 | 字符串 / `""` | P0 |
| `SISMEMBER` | Set 成员判定 | `"1"` / `"0"` | P0 |
| `BF.MADD` | Bloom filter 批量添加 | — | P1 |
| `BF.RESERVE` | Bloom filter 创建 | — | P1 |

## 连接池管理

```
wp-knowledge 内部
├── HashMap<endpoint_url, RedisPool>
│   ├── "redis://127.0.0.1:6379" → Pool(8 connections)
│   └── "redis://10.0.0.1:6379"  → Pool(4 connections)
└── HashMap<provider_name, endpoint_url>
    ├── "password_check" → "redis://127.0.0.1:6379"
    └── "known_actor"    → "redis://127.0.0.1:6379"  (共享同一 Pool)
```

`redis_exec("password_check", "BF.EXISTS", "weak_passwords", &[hash])` 的执行流程：

```
1. provider_name → endpoint_url
2. endpoint_url → Pool
3. 从 Pool 取连接 → 构造命令 → 执行 → 归还连接
```

## 错误处理

```rust
pub enum KnowledgeError {
    // 新增
    RedisConnectionError { endpoint: String, detail: String },
    RedisTimeout { name: String, duration: Duration },
    RedisUnsupportedCommand { name: String, command: String },
    RedisProviderNotFound { name: String },
    // 现有
    PostgresError { ... },
    ...
}
```

超时双层控制：连接超时（默认 `3s`）+ 命令超时（默认 `100ms`）。

## 与 wfusion 的集成

```
wf-runtime bootstrap
  │
  ├── init_postgres_provider(...)    # 现有
  └── init_redis_provider(...)       # 新增

wf-engine eval
  │
  └── ExternalCall { service, args }
        └── wp_knowledge::facade::redis_exec(service, cmd, key, &args)
             → "1" / "0" / "APT29"
             → wfusion 侧映射为 bool / chars
```

wfusion 不直接依赖 `redis` crate，全部通过 `wp_knowledge::facade` 访问。

## 依赖

```toml
[dependencies]
redis = { version = "0.25", features = ["tokio-comp", "connection-manager"] }
```

`ConnectionManager` 自带多路复用，无需自建连接池。

## 交付标准

- [ ] `init_redis_provider()` + `redis_exec()` API 可用
- [ ] `BF.EXISTS` / `HGET` / `GET` / `SISMEMBER` 四种命令验证通过
- [ ] 同 endpoint 连接池去重
- [ ] 连接超时 + 命令超时 + 错误返回
- [ ] `redis_ping()` + `redis_close()` 可用
- [ ] 单元测试（需本地 Redis + RedisBloom）
- [ ] 集成测试：wfusion `external()` 通过 `redis_exec()` 调用 Redis

## 相关文档

- External Function 设计 → warp-fusion: `docs/design/external-function-design.md`
