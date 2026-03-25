# wp-knowledge

`wp-knowledge` 是一个 KnowDB 组件，默认负责把目录化的 CSV + SQL 规则装载为 SQLite 查询库，并提供统一查询门面、线程克隆只读副本和若干内置 UDF；也支持把查询门面切换到外部 PostgreSQL。

这个目录已经按独立仓库形态整理，可直接作为 `https://github.com/wp-labs/wp-knowledge` 的仓库根使用；保留在 `wp-motor` workspace 内时也可以继续正常构建。

## 能力概览

- 读取 `knowdb.toml`，按表配置将 `create.sql`、`insert.sql` 和 `data.csv` 装载为权威 SQLite 库。
- 提供 `facade` 统一查询接口，支持无参、命名参数、白名单字典表读取和缓存查询。
- 支持通过 `knowdb.toml` 中的 `[provider]` 切换到外部 PostgreSQL。
- 支持线程克隆只读连接与 WAL 文件库两种 Provider 初始化方式。
- 内置 `ip4_int`、`ip4_between`、`cidr4_contains`、`trim_quotes` 等 SQLite UDF。

## 目录结构

- `src/`：库实现与单元测试。
- `tests/`：集成测试。
- `knowdb/`：随包分发的示例 KnowDB。
- `docs/`：设计文档与后续重构方案。
- `.github/workflows/ci.yml`：独立仓库可直接使用的 CI。

## 设计文档

- [Provider 与 Cache 重构方案](docs/PROVIDER_CACHE_ARCHITECTURE.md)

## 快速开始

```toml
[dependencies]
wp-knowledge = "0.10.3"
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

## 外部 PostgreSQL

`wp-knowledge` 现在支持在 `knowdb.toml` 中声明外部 PostgreSQL provider。配置后：

- 不再构建本地 `authority.sqlite`
- 推荐通过 `facade::query_fields/cache_query_fields` 使用 provider-neutral 参数接口
- `facade::query_named/cache_query` 仍保留，作为兼容旧版 SQLite 参数调用方式的 wrapper

示例：

```toml
version = 2

[provider]
kind = "postgres"
connection_uri = "postgres://user:pass@127.0.0.1:5432/demo"
```

命名参数在 PostgreSQL 中会自动从 `:name` 重写为 `$1/$2/...`，调用层不需要改 SQL 写法。

推荐的新查询方式示例：

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

## 测试 PostgreSQL Provider

仓库里现在有两类 PostgreSQL 集成测试。

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
