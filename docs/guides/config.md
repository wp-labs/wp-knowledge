# KnowDB 配置说明

这份文档只回答 4 件事：

1. `knowdb.toml` 应该怎么写。
2. 哪些配置在什么模式下生效。
3. 路径和 `${VAR}` 是怎么解析的。
4. 哪些错误会直接导致初始化失败。

当前只支持 `version = 2`。

## 两种模式

`wp-knowledge` 有两种配置方式：

- 目录式 SQLite authority
  - 由 `[[tables]]` 定义表目录、SQL 文件和 CSV 数据
  - 初始化时会先构建 authority SQLite，再切到只读查询 provider
- 外部 Provider
  - 通过 `[provider]` 直接连接 PostgreSQL 或 MySQL
  - 不再构建本地 authority SQLite

怎么选：

- 你的数据是随包分发的静态 CSV，就用目录式 SQLite authority
- 你的数据已经在 PostgreSQL / MySQL 里，就用外部 Provider

## 最小示例

### 目录式 SQLite authority

```toml
version = 2
base_dir = "."

[default]
transaction = true
batch_size = 2000
on_error = "fail"

[csv]
has_header = true
delimiter = ","
encoding = "utf-8"
trim = true

[cache]
enabled = true
capacity = 1024
ttl_ms = 30000

[[tables]]
name = "example"
dir = "example"
enabled = true
columns.by_header = ["name", "pinying"]

[tables.expected_rows]
min = 5
max = 100
```

### 外部 PostgreSQL / MySQL

```toml
version = 2

[cache]
enabled = true
capacity = 1024
ttl_ms = 30000

[provider]
kind = "postgres"
connection_uri = "postgres://user:${DB_PASSWORD}@127.0.0.1:5432/demo"
pool_size = 8
```

把 `kind = "postgres"` 换成 `kind = "mysql"`，连接串换成 MySQL 格式即可。

## 什么配置会生效

### 目录式 SQLite authority 模式

会生效：

- `version`
- `base_dir`
- `[default]`
- `[csv]`
- `[cache]`
- `[[tables]]`

不会从 `knowdb.toml` 里读取：

- `authority_uri`

`authority_uri` 由宿主调用 `facade::init_thread_cloned_from_knowdb(...)` 时单独传入。

### 外部 PostgreSQL / MySQL 模式

会生效：

- `version`
- `[provider]`
- `[cache]`

仍会被解析、但不会进入导入流程：

- `base_dir`
- `[default]`
- `[csv]`
- `[[tables]]`

## 字段速查

### 顶层字段

| 字段 | 必填 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `version` | 是 | 无 | 目前只能是 `2` |
| `base_dir` | 否 | `"."` | 表目录根路径，相对 `knowdb.toml` 所在目录解析 |
| `provider` | 否 | 无 | 配置外部 PostgreSQL / MySQL |
| `tables` | 否 | `[]` | 目录式 SQLite authority 的表定义 |

### `[default]`

只影响目录式 SQLite authority 的导入过程。

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `transaction` | `true` | 按批事务导入 |
| `batch_size` | `2000` | 每批提交多少行；实际最小为 `1` |
| `on_error` | `"fail"` | `fail` 失败即停，`skip` 跳过坏行 |

### `[csv]`

只影响目录式 SQLite authority 的 CSV 读取。

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `has_header` | `true` | 首行是否为表头 |
| `delimiter` | `","` | 分隔符，建议只用单字符 |
| `encoding` | `"utf-8"` | 当前只支持 `utf-8` |
| `trim` | `true` | 读取时去掉两端空白 |

### `[cache]`

只控制 runtime 的 `result cache`，不影响 `local cache` 和 `metadata cache`。

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `enabled` | `true` | 关闭后，全局结果缓存退化为 `Bypass` |
| `capacity` | `1024` | 最大缓存条目数，不是字节数 |
| `ttl_ms` | `30000` | 缓存 TTL，单位毫秒 |

### `[provider]`

只在外部 Provider 模式下使用。

| 字段 | 必填 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `kind` | 是 | 无 | `postgres` 或 `mysql` |
| `connection_uri` | 是 | 无 | 数据库连接串，支持 `${VAR}` |
| `pool_size` | 否 | `8` | 连接池大小；`0` 也会回退到 `8` |

说明：

- 不建议在 `knowdb.toml` 里写 `kind = "sqlite_authority"`
- 目录式 SQLite authority 模式下，直接省略整个 `[provider]` 即可

### `[[tables]]`

只在目录式 SQLite authority 模式下使用。

| 字段 | 必填 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `name` | 是 | 无 | 逻辑表名，也是 `{table}` 的替换值 |
| `dir` | 否 | 与 `name` 相同 | 表目录，相对 `base_dir` 解析 |
| `data_file` | 否 | `"data.csv"` | 数据文件，相对表目录解析 |
| `enabled` | 否 | `true` | 是否导入该表 |

列映射必须至少配置一组：

- `columns.by_header = ["col1", "col2"]`
- `columns.by_index = [0, 1]`

优先级：

- `by_header` 非空时优先使用 `by_header`
- `by_header` 为空时才使用 `by_index`

`[tables.expected_rows]` 可选：

| 字段 | 说明 |
| --- | --- |
| `min` | 实际导入行数小于它会直接失败 |
| `max` | 实际导入行数大于它只告警，不失败 |

## 表目录约定

每个表目录通常长这样：

```text
knowdb/
  knowdb.toml
  example/
    create.sql
    insert.sql
    data.csv
  zone/
    create.sql
    insert.sql
    clean.sql
    data.scv
```

规则：

- `create.sql` 必需
- `insert.sql` 必需
- `data.csv` 是默认数据文件，也可以用 `data_file` 指向其他文件名
- `clean.sql` 可选；缺省时默认执行 `DELETE FROM {table}`
- SQL 文件中的 `{table}` 会被当前表名替换

## 路径和环境变量

### `${VAR}` 怎么展开

`knowdb.toml` 会在 TOML 反序列化前先做 `${VAR}` 展开，例如：

```toml
[provider]
kind = "mysql"
connection_uri = "mysql://root:${MYSQL_PASSWORD}@127.0.0.1:3306/demo"
```

注意：

- 数据来自 `init_thread_cloned_from_knowdb(..., dict)` 传入的 `EnvDict`
- 不是 `wp-knowledge` 自己去读进程环境变量

### 路径怎么解析

路径解析顺序如下：

1. `knowdb_conf` 是相对路径时，先相对调用方传入的 `root` 解析
2. `base_dir` 再相对 `knowdb.toml` 所在目录解析
3. `tables[n].dir` 再相对 `base_dir` 解析
4. `tables[n].data_file` 再相对表目录解析

## 初始化失败的常见原因

以下情况会直接失败：

- `version` 不是 `2`
- `create.sql`、`insert.sql` 或数据文件不存在
- `columns.by_header` 和 `columns.by_index` 都没配置
- `columns.by_header` 指向了不存在的表头
- `encoding` 不是 `utf-8`
- CSV 行解析失败且 `on_error = "fail"`
- 实际导入行数小于 `expected_rows.min`
- PostgreSQL / MySQL 初始化连接失败

以下情况只会告警，不会失败：

- 实际导入行数大于 `expected_rows.max`
- `on_error = "skip"` 时遇到坏行

## 建议写法

- 目录式 SQLite authority 模式下，不要写 `[provider]`
- `enabled` 写在 `[[tables]]` 层，不要写到 `[tables.expected_rows]` 下面
- `delimiter` 用单字符
- 使用 `columns.by_header` 时，保持 `csv.has_header = true`
- 外部 PostgreSQL / MySQL 场景下，把 `[cache]` 理解为 `result cache` 配置，不要把它当成所有缓存层的总开关
