# wp-knowledge

`wp-knowledge` 是一个 KnowDB 查询与 Provider 组件，支持从 `knowdb.toml` 加载知识库定义，并通过统一查询门面访问 SQLite、PostgreSQL 或 MySQL 数据源。

这个目录已经按独立仓库形态整理，可直接作为 `https://github.com/wp-labs/wp-knowledge` 的仓库根使用；保留在 `wp-motor` workspace 内时也可以继续正常构建。

## 能力概览

- 读取 `knowdb.toml`，按表配置将 `create.sql`、`insert.sql` 和 `data.csv` 装载为权威 SQLite 库。
- 提供 `facade` 统一查询接口，支持无参、命名参数和缓存查询。
- 支持通过 `knowdb.toml` 中的 `[provider]` 切换到外部 PostgreSQL 或 MySQL。
- 支持线程克隆只读连接与 WAL 文件库两种 Provider 初始化方式。
- 内置 `ip4_int`、`ip4_between`、`cidr4_contains`、`trim_quotes` 等 SQLite UDF。

## 目录结构

- `src/`：库实现与单元测试。
- `tests/`：集成测试。
- `knowdb/`：随包分发的示例 KnowDB。
- `docs/`：架构与使用说明。
- `.github/workflows/ci.yml`：独立仓库可直接使用的 CI。

## 更多说明

- [Provider 与 Cache 架构说明](docs/PROVIDER_CACHE_ARCHITECTURE.md)

## 快速开始

```toml
[dependencies]
wp-knowledge = "0.11.0"
```

```rust
use std::path::Path;

use orion_variate::EnvDict;
use wp_knowledge::facade;

let authority_uri = "file:/tmp/wp-knowledge.sqlite?mode=rwc&uri=true";
facade::init_thread_cloned_from_knowdb(
    Path::new("."),
    Path::new("knowdb/knowdb.toml"),
    authority_uri,
    &EnvDict::new(),
)?;
let row = facade::query_row("SELECT COUNT(*) AS total FROM example")?;
# Ok::<(), wp_error::Error2>(())
```

## 外部 PostgreSQL / MySQL

`wp-knowledge` 现在支持在 `knowdb.toml` 中声明外部 PostgreSQL 或 MySQL provider。配置后：

- 不再构建本地 `authority.sqlite`
- 推荐通过 `facade::query_fields/cache_query_fields` 使用 provider-neutral 参数接口
- `facade::query_named/cache_query` 仍保留，作为兼容旧版 SQLite 参数调用方式的 wrapper
- 可通过 `facade::runtime_snapshot()` 读取当前 provider、generation，以及 result/local/metadata cache 的运行时计数与占用情况
- 可通过 `facade::install_runtime_telemetry(...)` 安装外部 telemetry bridge，把 reload/cache/query 事件接到 Prometheus、`wp-stats` 或其他宿主监控系统
- 可通过 `[cache]` 控制 `result cache` 的开关、容量和 TTL；`local cache` 与 `metadata cache` 不受这个配置影响
- PostgreSQL / MySQL 现在也接入了 `metadata cache` 与对应 telemetry，重复查询相同 SQL 时会记录 metadata cache hit/miss

示例：

```toml
version = 2

[cache]
enabled = true
capacity = 1024
ttl_ms = 30000

[provider]
kind = "postgres"
connection_uri = "postgres://user:${SEC_PWD}@127.0.0.1:5432/demo"
pool_size = 8
```

```toml
version = 2

[cache]
enabled = true
capacity = 1024
ttl_ms = 30000

[provider]
kind = "mysql"
connection_uri = "mysql://user:${SEC_PWD}@127.0.0.1:3306/demo"
pool_size = 8
```

命名参数在 PostgreSQL 中会自动从 `:name` 重写为 `$1/$2/...`；MySQL provider 原生支持 `:name` 命名参数。调用层都不需要改 SQL 写法。

`[cache]` 目前只有 3 个配置项：

- `enabled`
  控制 `result cache` 总开关；设为 `false` 后，原本会走 `UseGlobal` 的查询也会被强制降级为 `Bypass`
- `capacity`
  控制 `result cache` 最大条目数，单位是 entries，不是字节；`1024` 表示最多缓存 1024 条查询结果
- `ttl_ms`
  控制 `result cache` 的 TTL，单位是毫秒；外部 PostgreSQL / MySQL 数据发生变化但宿主没有 reload engine 时，依赖这个 TTL 做兜底失效

3 类 cache 的边界如下：

- `result cache`
  缓存查询结果集，受 `[cache]` 控制
- `local cache`
  历史调用期局部缓存，只在单次调用范围内复用，不受 `[cache]` 控制
- `metadata cache`
  缓存列名等元数据，不受 `[cache]` 控制

失效机制说明：

- reload knowdb、替换 provider 或 generation 变化时，`result cache` 会整体失效
- 外部数据源内容变化但宿主没有 reload 时，`result cache` 依赖 `ttl_ms` 到期后重新取数
- 当前没有对外部 PostgreSQL / MySQL 自动做 CDC、表版本号探测或事件通知接入

查询示例：

```rust
use wp_knowledge::facade;
use wp_model_core::model::DataField;

let params = [DataField::from_chars(":name".to_string(), "令狐冲".to_string())];
let row = facade::query_fields(
    "SELECT pinying FROM example WHERE name=:name",
    &params,
)?;
# Ok::<(), wp_error::Error2>(())
```

Telemetry bridge 示例：

```rust
use std::sync::Arc;

use wp_knowledge::facade;
use wp_knowledge::telemetry::{
    CacheTelemetryEvent, KnowledgeTelemetry, QueryTelemetryEvent, ReloadTelemetryEvent,
};

struct MyTelemetry;

impl KnowledgeTelemetry for MyTelemetry {
    fn on_cache(&self, event: &CacheTelemetryEvent) {
        let _ = event;
    }

    fn on_reload(&self, event: &ReloadTelemetryEvent) {
        let _ = event;
    }

    fn on_query(&self, event: &QueryTelemetryEvent) {
        let _ = event;
    }
}

let _previous = facade::install_runtime_telemetry(Arc::new(MyTelemetry));
```

## 测试外部 Provider

仓库里现在内置 PostgreSQL 和 MySQL 的 `ignored` 集成测试。

依赖本机或外部现成 PostgreSQL 的手工测试：

```bash
docker compose -f tests/docker-compose.yml up -d
export WP_KDB_TEST_POSTGRES_URL='postgres://postgres:demo@127.0.0.1:5432/postgres'
cargo test --test postgres_provider -- --ignored --nocapture
docker compose -f tests/docker-compose.yml down -v
```

这条测试默认是 `ignored`。只有你显式运行它时才会执行；如果环境变量未设置，或者 PostgreSQL 不可达，测试会直接失败并给出明确原因。

如果你要连现成的 PostgreSQL，只需要把 `WP_KDB_TEST_POSTGRES_URL` 替换成自己的连接串。仓库内置 Compose 配置见 [tests/docker-compose.yml](tests/docker-compose.yml)。

也可以直接执行仓库脚本：

```bash
bash tests/test-postgres-provider.sh
```

默认会在测试结束后执行 `docker compose down -v`；如果你想保留数据库容器和数据卷，执行 `KEEP_DB=1 bash tests/test-postgres-provider.sh`。如果要覆盖默认连接串，执行 `TEST_URL='postgres://user:pass@127.0.0.1:5432/db' bash tests/test-postgres-provider.sh`。

依赖本机或外部现成 MySQL 的手工测试：

```bash
export WP_KDB_TEST_MYSQL_URL='mysql://root:demo@127.0.0.1:3306/demo'
cargo test --test mysql_provider -- --ignored --nocapture
```

这条测试同样默认是 `ignored`，只在显式执行时运行。

如果你想用仓库内置的 MySQL 容器，也可以直接执行：

```bash
bash tests/test-mysql-provider.sh
```

默认会在测试结束后执行 `docker compose down -v`；如果你想保留数据库容器和数据卷，执行 `KEEP_DB=1 bash tests/test-mysql-provider.sh`。如果要覆盖默认连接串，执行 `TEST_URL='mysql://user:pass@127.0.0.1:3306/db' bash tests/test-mysql-provider.sh`。

自包含的 `testcontainers` 测试：

```bash
cargo test --test postgres_testcontainers -- --ignored --test-threads=1
```

这条测试要求本机 Docker daemon 可用；首次执行通常还会拉取 PostgreSQL 镜像，不需要预先安装或手工准备 PostgreSQL。

## 开发

独立仓库下建议执行：

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features -- --test-threads=1
```

## 许可证

Apache-2.0，见 [LICENSE](LICENSE)。
