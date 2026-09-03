# KnowDB 配置说明

中文 | [English](../../en/guides/config.md)

只记三件事：

1. 当前只支持 `version = 2`
2. 只能二选一：目录式 SQLite authority，或外部 PostgreSQL / MySQL
3. `[cache]` 只控制 `result cache`

## 两种模式

- 静态 CSV 随包分发：用目录式 SQLite authority
- 数据已经在 PostgreSQL / MySQL：用外部 provider

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

单个数据库：

```toml
version = 2

[cache]
enabled = true
capacity = 1024
ttl_ms = 30000

[provider.sqldb]
kind = "postgres"
connection_uri = "postgres://user:${DB_PASSWORD}@127.0.0.1:5432/demo"
pool_size = 8
```

把 `kind = "postgres"` 换成 `kind = "mysql"`，连接串换成 MySQL 格式即可。

**多个数据库**：用数组形式 `[[provider.sqldb]]`，每个配置一个 `name`：

```toml
version = 2

[[provider.sqldb]]
name = "geo"
kind = "postgres"
connection_uri = "postgres://user:${GEO_DB_PASSWORD}@127.0.0.1:5432/geo_db"
pool_size = 8

[[provider.sqldb]]
name = "asset"
kind = "postgres"
connection_uri = "postgres://user:${ASSET_DB_PASSWORD}@127.0.0.1:5432/asset_db"
pool_size = 8
```

- 没有写 `name` 的（或单个 `[provider.sqldb]`）生效名为 `default`，作为默认库。
- 未带前缀的 OML 查询走默认库；带前缀的查询指定具体库：

```oml
country = select country_name from geo.public.ip_geo_city where ip_num = @ip_num;
-- 路由到 name="geo" 的库；发往 PostgreSQL 的 SQL 会剥离 geo. 前缀。
```

- `name` 仅允许 `[A-Za-z0-9_]`；重名会报配置错误。

**PostgreSQL 连接级 session 初始化（可选）**：给连接池的每条新连接下发固定 `SET`，用于稳定执行计划（如 IP 地理查询锁定 generic plan，避免参数化查询反复重新 planning）。仅 `kind = "postgres"` 支持：

```toml
[provider.sqldb.postgres_session]
plan_cache_mode = "force_generic_plan"   # auto / force_generic_plan / force_custom_plan
jit = false                               # 关闭 JIT，小查询避免 JIT 开销
application_name = "ip_geo_service"       # 便于 PG 侧监控定位（≤ 63 字节）
```

- 三项均可省略：省略即不下发，保持数据库默认。
- 生效时机：建池后对池中**每条新连接**（含空闲回收补建、断线重连）在 `after_connect` 逐条执行 `SET`；初始化时再经同一连接池读取 `current_setting` 与期望值比对，不一致报配置错误并定位到具体参数。
- `plan_cache_mode` 需要 PostgreSQL 12+；`jit` / `application_name` 更早版本可用。
- 该配置段拒绝未知字段（`deny_unknown_fields`），不提供任意 SQL 注入入口。

## 哪些配置会生效

| 模式 | 会生效 | 不参与主流程 |
| --- | --- | --- |
| 目录式 SQLite authority | `version` `base_dir` `[default]` `[csv]` `[cache]` `[[tables]]` | `authority_uri` 不从 `knowdb.toml` 读取 |
| 外部 PostgreSQL / MySQL | `version` `[provider.sqldb]`（或 `[[provider.sqldb]]`） `[cache]` | `base_dir` `[default]` `[csv]` `[[tables]]` |
| 内网网段知识 `[intranet_nets]` | 两种模式均生效（随 knowdb.toml 解析注入） | — |

## 关键字段速查

### 顶层

- `version`
  - 必填，只能是 `2`
- `base_dir`
  - 表目录根路径，相对 `knowdb.toml` 所在目录解析
- `[provider.sqldb]` / `[[provider.sqldb]]`
  - 只在外部 provider 模式下使用
- `[[tables]]`
  - 只在目录式 SQLite authority 模式下使用

### `[default]`

- `transaction`
  - 默认 `true`
- `batch_size`
  - 默认 `2000`
- `on_error`
  - `fail` 直接失败
  - `skip` 跳过坏行

### `[csv]`

- `has_header`
  - 默认 `true`
- `delimiter`
  - 建议单字符
- `encoding`
  - 目前只支持 `utf-8`
- `trim`
  - 默认 `true`

### `[intranet_nets]`

内网网段知识配置（供 `intranet_ip` / `access_direct` 判断内/外网）。

- `enabled`
  - 默认 `true`（提供配置即生效）
  - 设 `false` 忽略本节，使用内置默认网段
- `mode`
  - `add`：外部网段添加到内置默认网段（默认，推荐）
  - `replace`：外部网段完全替换内置默认网段
- `nets`
  - 内网网段列表（CIDR 写法，支持 IPv4 / IPv6）
  - 示例：`nets = ["172.32.0.0/16"]`

**默认内置网段**：RFC1918（10/8、172.16/12、192.168/16）+ IPv4/IPv6 loopback + IPv6 ULA（fc00::/7）。CGNAT、link-local 等特殊地址默认不判为内网，可按需配置。

**示例**
```toml
[intranet_nets]
enabled = true
mode = "add"
nets = ["172.32.0.0/16"]
```

### `[cache]`

- `enabled`
  - 默认 `true`
- `capacity`
  - 默认 `1024`，单位是条目数
- `ttl_ms`
  - 默认 `30000`

### `[provider.sqldb]`

- `name`
  - 可选；多库时必须写，用于 OML 查询前缀路由
  - 未写时生效名为 `default`
  - 仅允许 `[A-Za-z0-9_]`，重名会报错
- `kind`
  - 必填，`postgres` 或 `mysql`
- `connection_uri`
  - 必填，支持 `${VAR}`
- `pool_size`
  - 可选，默认 `8`
- `postgres_session`
  - 可选；PostgreSQL 连接级 session 初始化，见下节
  - 仅 `kind = "postgres"` 合法，其他 kind 配置该段会在加载期报错

不要写 `kind = "sqlite_authority"`。目录式模式下直接省略 `[provider.sqldb]`。

### `[provider.sqldb.postgres_session]`

PostgreSQL 专属的连接级 session 初始化（稳定执行计划用）；仅 `kind = "postgres"` 合法。

- `plan_cache_mode`
  - 可选，`auto` / `force_generic_plan` / `force_custom_plan`
  - 需要 PostgreSQL 12+；IP 地理等参数化查询建议 `force_generic_plan`
- `jit`
  - 可选，`true` / `false`
  - 小查询建议 `false`，避免 JIT 编译开销
- `application_name`
  - 可选，≤ 63 字节、无控制字符；含单引号自动转义
  - 便于 PG 侧 `pg_stat_activity` 监控定位
- 未配置的项不下发，保持数据库默认；整个子配置缺省时连接池行为与默认完全一致

### `[[tables]]`

- `name`
  - 必填
- `dir`
  - 默认等于 `name`
- `data_file`
  - 默认 `data.csv`
- `enabled`
  - 默认 `true`
- 列映射至少配置一组：
  - `columns.by_header`
  - `columns.by_index`
- 优先级：
  - `by_header` 优先
  - `by_header` 为空时才用 `by_index`

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
- `data.csv` 是默认数据文件
- `clean.sql` 可选；缺省时默认执行 `DELETE FROM {table}`
- SQL 文件中的 `{table}` 会被当前表名替换

## 路径和环境变量

- `${VAR}` 会在 TOML 反序列化前展开，例如：

```toml
[provider.sqldb]
kind = "mysql"
connection_uri = "mysql://root:${MYSQL_PASSWORD}@127.0.0.1:3306/demo"
```

- 值来自 `init_thread_cloned_from_knowdb(..., dict)` 传入的 `EnvDict`
- 不是 `wp-knowledge` 直接读进程环境变量

路径解析顺序：

1. `knowdb_conf` 是相对路径时，先相对调用方传入的 `root` 解析
2. `base_dir` 再相对 `knowdb.toml` 所在目录解析
3. `tables[n].dir` 再相对 `base_dir` 解析
4. `tables[n].data_file` 再相对表目录解析

## 常见失败原因

- `version` 不是 `2`
- `create.sql`、`insert.sql` 或数据文件不存在
- `columns.by_header` 和 `columns.by_index` 都没配置
- `columns.by_header` 指向了不存在的表头
- `encoding` 不是 `utf-8`
- CSV 行解析失败且 `on_error = "fail"`
- 实际导入行数小于 `expected_rows.min`
- PostgreSQL / MySQL 初始化连接失败
- `postgres_session` 与数据库不匹配（如 PG 11 上配置 `plan_cache_mode`），或启动自检 `current_setting` 与期望不符

## 建议写法

- 目录式 SQLite authority 模式下，不要写 `[provider.sqldb]`
- `enabled` 写在 `[[tables]]` 层，不要写到 `[tables.expected_rows]` 下面
- `delimiter` 用单字符
- 使用 `columns.by_header` 时，保持 `csv.has_header = true`
- 外部 PostgreSQL / MySQL 场景下，把 `[cache]` 理解为 `result cache` 配置
