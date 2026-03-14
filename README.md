# wp-knowledge

`wp-knowledge` 是一个围绕 SQLite 的 KnowDB 组件，负责把目录化的 CSV + SQL 规则装载为查询库，并提供统一查询门面、线程克隆只读副本和若干内置 UDF。

这个目录已经按独立仓库形态整理，可直接作为 `https://github.com/wp-labs/wp-knowledge` 的仓库根使用；保留在 `wp-motor` workspace 内时也可以继续正常构建。

## 能力概览

- 读取 `knowdb.toml`，按表配置将 `create.sql`、`insert.sql` 和 `data.csv` 装载为权威 SQLite 库。
- 提供 `facade` 统一查询接口，支持无参、命名参数、白名单字典表读取和缓存查询。
- 支持线程克隆只读连接与 WAL 文件库两种 Provider 初始化方式。
- 内置 `ip4_int`、`ip4_between`、`cidr4_contains`、`trim_quotes` 等 SQLite UDF。

## 目录结构

- `src/`：库实现与单元测试。
- `tests/`：集成测试。
- `knowdb/`：随包分发的示例 KnowDB。
- `.github/workflows/ci.yml`：独立仓库可直接使用的 CI。

## 快速开始

```toml
[dependencies]
wp-knowledge = "1.19.2"
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

## 开发

独立仓库下建议执行：

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features -- --test-threads=1
```

## 许可证

Apache-2.0，见 [LICENSE](LICENSE)。
