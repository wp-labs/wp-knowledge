# PostgreSQL Provider Tasks

## Context

`wp-knowledge` 已经具备外部 PostgreSQL provider 的基础能力：

- `knowdb.toml` 支持 `[provider] kind = "postgres"`
- `facade::query_fields(...)` / `cache_query_fields(...)` 已成为推荐的 provider-neutral 查询入口
- `facade::query_named(...)` / `cache_query(...)` 仍保留，作为兼容旧版 SQLite 参数接口的 wrapper
- PostgreSQL 的基础查询、命名参数重写、`query_cipher`、手工测试脚本已经可用

当前状态适合验证功能通路，但还不足以视为“长期稳定可运营”的 PostgreSQL 支持。

## Priority 1

### 1. Replace single-client provider with a connection pool

Current:

- `src/postgres.rs` 使用 `Mutex<Client>`，所有查询串行化

Tasks:

- 评估并接入 PostgreSQL 连接池
- 把 provider 从单连接改为池化获取连接
- 为连接失败、连接断开、重试策略定义清晰行为

Acceptance:

- 并发查询不依赖单连接串行执行
- 基础测试和 PostgreSQL 集成测试全部通过

### 2. Add provider startup validation

Current:

- provider 初始化阶段只检查能否连接
- 表存在性、权限、schema 偏差要到实际查询时才暴露

Tasks:

- 初始化时增加基础自检
- 校验白名单表是否存在
- 校验 `query_cipher` 依赖表是否可读
- 输出清晰的错误信息，区分连接问题、权限问题、schema 问题

Acceptance:

- 错库、错 schema、错权限能在初始化阶段直接失败

## Priority 2

### 3. Define SQL/UDF compatibility boundary

Current:

- PostgreSQL 目前只支持基础 SQL 查询和命名参数重写
- SQLite UDF 体系没有迁移到 PostgreSQL

Tasks:

- 梳理当前 `wp-knowledge`/`wp-oml` 常见查询中依赖的 SQLite UDF
- 明确哪些 SQL 能直接迁到 PostgreSQL
- 明确哪些 SQL 需要改写，或需要用户自行在 PostgreSQL 中提供函数
- 把边界写进文档和示例

Acceptance:

- 对外有清晰列表说明“可直接使用”和“需要改造”的 SQL 能力

### 4. Expand PostgreSQL type coverage

Current:

- `src/postgres.rs` 只支持一部分返回列类型
- 绑定参数时，很多 `DataField` 类型会退化成文本

Tasks:

- 补充更完整的 PostgreSQL 列类型映射
- 评估并补充更合理的参数绑定类型
- 对不支持类型给出更具体的错误提示

Acceptance:

- 常用数值、时间、JSON、网络相关类型具备明确支持策略

## Priority 3

### 5. Complete provider-neutral API migration

Current:

- 新代码可用 `query_fields(...)` / `cache_query_fields(...)`
- 兼容接口仍暴露 `rusqlite::ToSql`

Tasks:

- 在仓库内持续把新调用迁到 provider-neutral API
- 评估是否需要给兼容接口加 `deprecated` 标记
- 制定 `query_named(...)` / `cache_query(...)` 的长期保留或移除策略

Acceptance:

- 新代码路径默认不依赖 SQLite 类型
- 兼容层定位清晰

### 6. Improve automated verification

Current:

- `tests/postgres_provider.rs` 依赖显式环境准备
- `tests/postgres_testcontainers.rs` 目前为 `ignored`
- `tests/test-postgres-provider.sh` 可用于本地 Docker Desktop 手工验证

Tasks:

- 设计 CI 中的 PostgreSQL 自动验证方式
- 决定继续采用 Compose 还是启用 `testcontainers`
- 保证失败时能输出足够的连接、容器、SQL 诊断信息

Acceptance:

- 在标准开发/CI 流程中可以稳定验证 PostgreSQL provider

## Priority 4

### 7. Add end-to-end example in consuming repo

Current:

- `wp-knowledge` 本身已经支持 PostgreSQL provider
- 但 `wp-motor`/`wp-oml` 侧还缺一条完整、可复现的 PostgreSQL 示例链路

Tasks:

- 在消费侧仓库补一份 PostgreSQL knowdb 配置示例
- 用真实 schema 和查询验证 `wp-oml` 调用链
- 补充运行说明，确保团队成员可直接复用

Acceptance:

- 从消费侧项目可以完整验证 PostgreSQL provider 接入链路

## Suggested execution order

1. 连接池
2. 初始化自检
3. SQL/UDF 能力边界文档
4. 类型覆盖增强
5. 自动化验证
6. 消费侧端到端示例
7. 兼容接口废弃策略
