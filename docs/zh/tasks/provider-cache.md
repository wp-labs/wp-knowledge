# Provider / Cache Refactor Tasks

中文 | [English](../../en/tasks/provider-cache.md)

## Context

`wp-knowledge` 现在已经同时支持：

- 本地 SQLite authority / thread-cloned provider
- 外部 PostgreSQL provider
- 外部 MySQL provider

但 provider 与 cache 架构仍然主要沿用 SQLite 历史实现：

- `facade` 虽然抽象出 `QueryFacade`，但兼容层仍带有 `rusqlite::ToSql` 痕迹
- 结果缓存仍以调用方传入的局部 `FieldQueryCache` 为主
- `COLNAME_CACHE` 仍是进程级、无上限、无 generation 的 `HashMap`
- `ThreadClonedMDB` 的线程本地副本没有统一版本语义
- provider 生命周期仍缺少正式的 reload / replace 机制

对应架构方案见：

- [provider-cache.md](../architecture/provider-cache.md)

当前状态适合继续扩展数据库接入，但还不足以支撑“多 provider + 可控缓存 + reload”的长期演进。

## Execution rule

由于 `wp-knowledge` 已经持续演进，在执行下面每一个任务前，必须先重新评估当前代码，而不是直接按本文档假设开工。

执行要求：

- 先阅读当前相关实现，确认哪些能力已经落地、哪些问题已经被部分解决
- 将“文档中的目标状态”和“当前代码实际状态”做一次 gap 分析
- 如果代码已经覆盖部分任务，应先缩小任务范围，再继续实现
- 如果现有实现与架构文档发生偏移，应优先更新任务拆分与验收标准，再进入编码
- 每个任务的验收都应以“当前代码基线上的改进”为准，而不是以文档编写时的旧状态为准

建议执行顺序：

1. 评估当前代码
2. 标记已完成 / 部分完成 / 未开始
3. 收缩当前任务范围
4. 再进入实现、测试、文档更新
5. 任务完成后执行自 review
6. 修复 review 发现的问题后再收尾

## Completion rule

每次任务完成后，不应直接结束，必须追加一次 review，并处理 review 发现的问题。

执行要求：

- 先以 code review 视角检查本次改动
- 优先查行为回归、边界条件、命名问题、测试缺口、文档遗漏
- 如果 review 发现问题，先修复，再重新验证
- 只有在 review 后没有遗留问题，任务才算完成

最小闭环应为：

1. 实现
2. 测试 / 验证
3. review
4. 修复 review 问题
5. 再验证
6. 更新必要文档

## Current status

基于 2026-03-25 当前代码基线，任务状态更新如下：

- 已完成：
  - `KnowledgeRuntime` 已引入，provider 已改为可替换安装，并显式携带 `datasource_id + generation`
  - facade 已统一经 runtime 分发，`query/query_row/query_fields/cache_query` 已接入统一执行路径
  - 已引入统一 `QueryRequest / QueryParam / QueryResponse / CachePolicy`
  - 已增加 bounded global result cache，key 已带 `datasource_id + generation + query_hash + params_hash + mode`
  - `FieldQueryCache` 已带 generation 失效语义，并提供 `QueryLocalCache` 别名
  - `ThreadClonedMDB` 已具备 generation-aware 的线程本地快照重建能力
  - SQLite metadata cache 已按 `datasource_id + generation + query_hash` 做自然失效，并由 bounded `LruCache` 承载

- 部分完成：
  - reload / observability 已补充基础日志、runtime snapshot、cache/reload 计数器、installable telemetry hook 与“失败保留旧 provider”的回退行为；当前剩余的是宿主侧 exporter/adapter 接入与更统一的诊断出口
  - 兼容层仍保留 `query_named(...)` / `cache_query(...)`，但仓库内 provider-neutral 入口迁移与淘汰策略尚未完全结束

- 未开始或未完全展开：
  - 宿主侧 metrics/telemetry adapter（如 Prometheus / `wp-stats` bridge）
  - compatibility surface 的弃用策略

## Priority 1

### 1. Introduce replaceable runtime shell

Current:

- facade 仍直接持有全局 provider
- provider 生命周期没有正式的 replace / reload 语义

Tasks:

- 新增 `KnowledgeRuntime`
- 把当前全局 provider 提升为 runtime 持有的 provider handle
- provider handle 显式携带 `datasource_id` 与 `generation`
- facade 统一经 runtime 分发

Acceptance:

- provider 不再依赖一次性 `OnceLock` 绑定
- 内部具备后续做 reload / replace 的基础结构

### 2. Define provider-neutral request / result model

Current:

- SQLite / PostgreSQL / MySQL provider 都有各自的参数绑定路径
- facade 兼容层仍暴露 SQLite 风格参数接口

Tasks:

- 引入 `QueryRequest` / `QueryParam` / `QueryResult`
- 新增内部 `KnowledgeProvider::execute(...)`
- SQLite / PostgreSQL / MySQL 迁移到统一请求模型
- 保留 facade 兼容入口，但内部不再直接依赖 `rusqlite::ToSql`

Acceptance:

- provider 差异收敛到各自实现内部
- runtime 层只处理统一请求模型

## Priority 2

### 3. Replace `COLNAME_CACHE` with bounded metadata cache

Current:

- `COLNAME_CACHE` 只按 SQL 文本缓存列名
- 不区分 provider / generation
- 没有容量上限

Tasks:

- 引入统一 metadata cache
- key 至少包含 `datasource_id + generation + query_hash`
- value 先覆盖当前列名缓存能力
- 增加容量上限，必要时增加 TTL

Acceptance:

- 不再保留进程级无边界 `HashMap` 列名缓存
- metadata cache 能随 provider / generation 自然失效

### 4. Introduce global result cache with explicit policy

Current:

- 结果缓存主要由调用方显式传入 `FieldQueryCache`
- cache key 缺少 provider 身份和版本信息

Tasks:

- 引入统一全局结果缓存
- 定义 `CachePolicy`，至少支持 `Bypass / UseGlobal / UseCallScope`
- 结果缓存 key 带上 `datasource_id + generation + query_hash + params_hash + mode`
- 首批只对显式允许缓存的查询启用

Acceptance:

- 全局结果缓存不会在不同 provider / generation 之间串数据
- 默认查询行为仍然保守，不会隐式过度缓存

## Priority 3

### 5. Re-scope local cache as compatibility layer

Current:

- `FieldQueryCache` 名义上像主缓存，实际上只适合单次计算内去重

Tasks:

- 将 `FieldQueryCache` 重新定位为 local cache
- 评估并改名为 `QueryLocalCache`
- 明确 `cache_query` 的执行顺序：local cache -> global cache -> provider query
- 保持现有 OML/规则路径可继续工作

Acceptance:

- local cache 与 global cache 语义清晰分层
- `cache_query` 不再承担全局一致性责任

### 6. Make `ThreadClonedMDB` generation-aware

Current:

- 线程第一次访问后会持有本地副本
- provider reload 或 authority 重建后，旧线程副本不会自动失效

Tasks:

- 为线程本地状态引入 `generation`
- 当前 generation 变化时自动丢弃旧副本并重建
- 把 thread local snapshot 纳入 runtime provider handle 体系

Acceptance:

- authority 重建后线程读取会自动切换到新快照
- provider 切换后不会继续读旧 SQLite 副本

## Priority 4

### 7. Define reload and observability contract

Current:

- provider 切换与 cache 失效缺少统一观测面

Tasks:

- 定义 runtime reload 流程
- 增加 provider / cache / generation 的关键日志与指标
- 明确 reload 失败时的行为与回退策略

Acceptance:

- 能回答“当前查询命中了哪个 provider / 哪层 cache / 哪个 generation”
- reload 成功与失败有明确诊断信息

### 8. Clean up compatibility surface

Current:

- facade 兼容 API 仍有历史负担

Tasks:

- 梳理 `query_named(...)` / `cache_query(...)` 的长期定位
- 评估是否给 SQLite-shaped 兼容接口加 `deprecated`
- 在仓库内持续迁移到 provider-neutral 入口

Acceptance:

- 新代码默认走 provider-neutral API
- 兼容层保留范围和淘汰路径清晰

## Suggested execution order

1. replaceable runtime shell
2. provider-neutral request / result model
3. metadata cache
4. global result cache
5. local cache re-scope
6. generation-aware thread clone
7. reload / observability
8. compatibility cleanup

## Notes

- 第一阶段不要追求“一次性删干净旧接口”，先把 runtime 与 generation 语义建立起来
- PostgreSQL / MySQL 已接入不代表架构问题已解决；它们只是让这次重构的必要性更高
- `ThreadClonedMDB` 是 correctness 关键路径，不应被推迟到最后才处理
