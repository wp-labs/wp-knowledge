# wp-knowledge

> 数据驱动的 **KnowDB 查询与 Provider 组件**：一份 `knowdb.toml` 同时定义“数据从哪来、怎么装载、查哪个库”，并以统一查询门面访问 **SQLite（权威库）/ PostgreSQL / MySQL**。

`wp-knowledge` 既可作为独立仓库使用（`github.com/wp-labs/wp-knowledge`），也保留在 `wp-motor` workspace 内正常构建。

| | |
|---|---|
| 版本 | `0.17.0`（crate `wp-knowledge`，edition 2024） |
| 许可证 | Apache-2.0 |
| 文档 | [Docs Index](docs/README.md) · [中文](docs/zh/README.md) · [English](docs/en/README.md) |

---

## ✨ 特性

- **声明式装载**：读取 `knowdb.toml`，按表配置把 `create.sql / insert.sql / data.csv` 装载为权威 SQLite 库（`loader` 负责类型化重灌与投影）。
- **统一查询门面 `facade`**：一套 API 覆盖 SQLite / PostgreSQL / MySQL——无参、命名参数、缓存查询；PG 的 `:name` 自动改写为 `$1/$2/...`，MySQL 原生支持 `:name`，业务层不改 SQL。
- **分层缓存**：`result cache`（结果集，可配容量/TTL）、`local cache`（单次调用局部）、`metadata cache`（列元数据），reload / generation 变化整体失效。
- **定期刷新（换代 + 信号 + 函数取数）**：`RefreshService` 按表周期重载，把最新一代换入共享 `TableStore`（O(1) Arc 指针换代），只发纯信号 `RefreshSignal`；调用者以 `store.snapshot` 函数 pull，Arc 零复制、丢信号无害。
- **VEL 变量求值**：NamedSql 的 SQL 模板支持 `$name` 占位符，由 VEL 代码按 knowdb 时钟每 tick 求值替换（如相位标签 `phase_now/phase_next`）。
- **Provider 初始化两形态**：线程克隆只读连接、WAL 文件库。
- **内置 SQLite UDF**：`ip4_int`、`ip4_between`、`cidr4_contains`、`trim_quotes` 等。
- **可观测**：`runtime_snapshot()` 读 provider/generation/缓存计数；telemetry bridge 把 reload / query / cache 事件接到 Prometheus、`wp-stats` 或宿主监控。

## 📦 快速开始

```toml
[dependencies]
wp-knowledge = "0.17.0"
```

```rust
use std::path::Path;

use orion_variate::EnvDict;
use wp_knowledge::facade;

let authority_uri = "file:/tmp/wp-knowledge.sqlite?mode=rwc&uri=true";
facade::init_thread_cloned_from_knowdb(
    Path::new("."),
    Path::new("knowdb/knowdb.toml"), // 仓库自带示例 KnowDB
    authority_uri,
    &EnvDict::new(),
)?;
let row = facade::query_row("SELECT COUNT(*) AS total FROM example")?;
# Ok::<(), wp_error::Error2>(())
```

> 配置语法与完整查询示例见 [配置指南](docs/zh/guides/config.md)（[EN](docs/en/guides/config.md)）。

## 🧭 核心概念

```text
knowdb.toml ──► loader：装载权威 SQLite（create/insert/data.csv）
                   │
                   ▼
            facade（统一查询门面）──► provider
                                      ├─ SQLite 权威库（默认，本地产物 authority.sqlite）
                                      ├─ PostgreSQL（命名参数 :name → $1/$2...）
                                      └─ MySQL（原生 :name）
```

### 数据装载与权威库

`loader` 按 `knowdb.toml` 的表目录生成类型化 SQL 并重灌权威 SQLite 库；每张表可声明
`refresh` 周期，由刷新服务周期重载（见下）。装载失败/表禁用/空表都有明确错误语义，
集成在 `loader::reload_table_rows` 单测中覆盖。

### 外部 PostgreSQL / MySQL

在 `knowdb.toml` 声明 `[provider]` 后**不再构建本地 authority.sqlite**：

```toml
version = 2

[cache]
enabled = true
capacity = 1024
ttl_ms = 30000

[provider]
kind = "postgres"            # 或 "mysql"
connection_uri = "postgres://user:${SEC_PWD}@127.0.0.1:5432/demo"
pool_size = 8
```

- 推荐 `facade::query_fields / cache_query_fields`（provider-neutral 参数接口）；
  `query_named / cache_query` 保留为兼容旧 SQLite 参数的 wrapper。
- `[cache]` 仅控制 **result cache**：`enabled`（总开关，false → `UseGlobal` 降级
  `Bypass`）、`capacity`（条目数）、`ttl_ms`（外部数据变化而宿主未 reload 时的兜底失效）。
  `local cache` 与 `metadata cache` 不受其控制。
- reload / provider 替换 / generation 变化 → result cache 整体失效；外部数据源目前
  **不做** CDC、表版本探测或事件通知。

```rust
use wp_knowledge::facade;
use wp_model_core::model::DataField;

let params = [DataField::from_chars(":name".to_string(), "令狐冲".to_string())];
let row = facade::query_fields("SELECT pinying FROM example WHERE name=:name", &params)?;
# Ok::<(), wp_error::Error2>(())
```

#### Telemetry

```rust
use std::sync::Arc;

use wp_knowledge::facade;
use wp_knowledge::telemetry::{
    CacheTelemetryEvent, KnowledgeTelemetry, QueryTelemetryEvent, ReloadTelemetryEvent,
};

struct MyTelemetry;

impl KnowledgeTelemetry for MyTelemetry {
    fn on_cache(&self, event: &CacheTelemetryEvent) { let _ = event; }
    fn on_reload(&self, event: &ReloadTelemetryEvent) { let _ = event; }
    fn on_query(&self, event: &QueryTelemetryEvent) { let _ = event; }
}

let _previous = facade::install_runtime_telemetry(Arc::new(MyTelemetry));
```

### 定期刷新：换代、信号与函数取数

```text
调用者(boot)                    knowdb(RefreshService + TableStore)        调用者(daemon)
    │  register RefreshSpec ───────────▶│                                      │
    │  load_rows 同步装载(seed)          ├─ tick: reload(spec)                 │
    │                                    │   └─ 成功 → store 换代(Arc)          │
    │                                    │             + 信号 {name} ─────────▶│ snapshot(name)
    │                                    │                                      │ 函数 pull → 搬入
```

- 每表一条 `RefreshSpec` 独立计时；tick 重载成功 → `TableStore::insert` **O(1) Arc 换代** → 纯信号；
- 调用者以 `TableStore::snapshot` **函数取数**（Arc 零复制、不可变代共享、信号可丢无害）；
- 同名 spec 自动去重（保留首个）；失败跳周期、保留上一代；
- NamedSql 的 `code` 块用 **VEL** 按 knowdb 时钟求值替换 `$name`（内建 `phase_now/phase_next`）。

> 完整机制、宿主接入样板与边界语义见 [定期刷新与 VEL 指南](docs/zh/guides/refresh.md)（[EN](docs/en/guides/refresh.md)）。

## 📚 文档

| 主题 | 中文 | English |
|---|---|---|
| 文档索引 | [docs/zh](docs/zh/README.md) | [docs/en](docs/en/README.md) |
| KnowDB 配置 | [config](docs/zh/guides/config.md) | [config](docs/en/guides/config.md) |
| 定期刷新与 VEL | [refresh](docs/zh/guides/refresh.md) | [refresh](docs/en/guides/refresh.md) |
| Provider 与 Cache 架构 | [provider-cache](docs/zh/architecture/provider-cache.md) | [provider-cache](docs/en/architecture/provider-cache.md) |
| Async Provider 性能 | [async-provider](docs/zh/performance/async-provider.md) | [async-provider](docs/en/performance/async-provider.md) |

## 🧪 测试外部 Provider

PostgreSQL / MySQL 集成测试默认 `ignored`，需要显式运行。

**自备数据库（任意 host）：**

```bash
export WP_KDB_TEST_POSTGRES_URL='postgres://postgres:demo@127.0.0.1:5432/postgres'
cargo test --test postgres_provider -- --ignored --nocapture

export WP_KDB_TEST_MYSQL_URL='mysql://root:demo@127.0.0.1:3306/demo'
cargo test --test mysql_provider -- --ignored --nocapture
```

**内置 Compose / 一键脚本（含 PG 与 MySQL 的 correctness / perf）：**

```bash
bash tests/test-postgres-provider-correctness.sh
bash tests/test-postgres-provider-perf.sh
bash tests/test-mysql-provider-correctness.sh
bash tests/test-mysql-provider-perf.sh
```

- 默认结束后 `docker compose down -v`；保留容器/数据卷加 `KEEP_DB=1`，覆盖连接串加
  `TEST_URL=...`，并行跑多个 provider 用不同 `COMPOSE_PROJECT_NAME`；
- 自包含 testcontainers：`cargo test --test postgres_testcontainers -- --ignored --test-threads=1`
  （要求本机 Docker daemon，首次会自动拉取镜像）。

## 🔧 开发

独立仓库下建议：

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features -- --test-threads=1
```

## 📄 许可证

Apache-2.0，见 [LICENSE](LICENSE)。相关工程：[warp-parse 技术栈](https://github.com/wp-labs)。
