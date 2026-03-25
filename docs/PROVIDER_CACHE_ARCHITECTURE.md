# wp-knowledge Provider 与 Cache 重构方案

## 背景

`wp-knowledge` 当前已经同时支持两类数据源：

- 本地 KnowDB 构建出的 SQLite authority 库
- 外部 PostgreSQL

但现有实现仍然带有明显的 SQLite 历史包袱，尤其体现在：

- `facade` 虽然抽象出了 `QueryFacade`，但部分接口仍围绕 `rusqlite::ToSql` 组织
- 查询缓存由调用方传入 `FieldQueryCache`，生命周期和 key 语义不统一
- `COLNAME_CACHE` 是进程级无上限 `HashMap`
- `ThreadClonedMDB` 使用线程本地内存副本，reload 后没有统一的刷新语义
- `PROVIDER` 和白名单通过 `OnceLock` 绑定，初始化后不可替换

这些设计在“单进程 + SQLite + 单次计算内 lookup 优化”的场景下可以工作，但在以下目标下已经不够：

- 支持多种外部数据库
- 支持可控的缓存失效
- 支持 provider reload / 切换
- 支持统一的查询与元数据缓存
- 在不破坏现有 OML / WPL 调用方式的前提下演进

## 目标

本次重构目标：

- 建立数据源无关的 provider 抽象
- 将查询缓存从“调用期局部缓存”升级为“统一策略缓存”
- 为 provider reload、schema 变更、切换外部库引入明确失效语义
- 保留现有 `facade::query_row/query_named/cache_query` 的兼容入口
- 为 SQLite / PostgreSQL / 后续外部库提供统一接入模型

## 非目标

本方案不试图在第一阶段解决以下问题：

- 分布式缓存
- 跨进程缓存一致性
- 任意 SQL 自动分析是否可缓存
- 跨数据库 SQL 方言统一
- 事务内读写一致性抽象

第一阶段目标是把进程内架构理顺，让 provider、cache、reload 语义清晰且可扩展。

## 当前问题摘要

### 1. 结果缓存语义过弱

当前 `cache_query` 的 key 和生命周期都由调用方决定：

- 相同 SQL 在不同 provider 上可能命中同一个 key
- reload 后如果调用方仍持有旧 cache，可能继续返回旧结果
- key 只依赖 `DataField`，不携带 provider 身份和版本

### 2. 元数据缓存无边界

`COLNAME_CACHE` 只按 SQL 文本缓存列名，存在这些问题：

- 没有容量上限
- 没有 TTL
- 不区分 provider
- 不区分 schema/generation

### 3. Provider 生命周期不可替换

`PROVIDER` 使用 `OnceLock`，意味着：

- 初始化后无法安全切换数据源
- reload 不具备正式机制
- provider 状态与 cache 状态无法一起更新

### 4. 线程本地副本缺少版本感知

`ThreadClonedMDB` 为每个线程缓存一个 authority 的内存副本，这本质上是“数据库快照缓存”。

当前缺少：

- reload 后线程副本自动失效
- provider 切换后旧线程副本自动重建
- provider 版本与线程本地状态的关联

## 总体设计

### 设计原则

- Provider 负责执行查询，不负责缓存策略
- Cache 是统一基础设施，不依赖具体数据库实现
- 所有缓存 key 都必须带上数据源身份与版本
- reload 的主失效机制采用 `generation`
- TTL 只作为兜底，不作为主一致性机制
- 兼容现有 facade API，但内部逐步迁移到新的请求模型

### 目标分层

建议将内部结构分为 4 层：

1. `DatasourceRegistry`
2. `KnowledgeProvider`
3. `QueryRuntime`
4. `Facade Compatibility Layer`

职责如下：

- `DatasourceRegistry`
  - 持有当前激活的数据源
  - 管理 `source_id`
  - 管理 `generation`
  - 处理 reload / replace

- `KnowledgeProvider`
  - 执行只读查询
  - 执行字典表读取
  - 可选返回 schema / metadata 信息

- `QueryRuntime`
  - 执行缓存判定
  - 统一生成 cache key
  - 持有结果缓存、元数据缓存
  - 提供 query plan / query request 执行

- `Facade Compatibility Layer`
  - 暴露现有 `query/query_row/query_named/query_cipher/cache_query`
  - 把老接口适配到新 runtime

## 核心抽象

### DatasourceId

每个 provider 实例必须有稳定身份：

```rust
pub struct DatasourceId(String);
```

建议取值：

- SQLite authority 文件路径 hash
- PostgreSQL 连接配置 hash
- 未来 HTTP / MySQL / ClickHouse 等数据源的配置 hash

`DatasourceId` 只用于区分数据源，不直接暴露凭据。

### Generation

引入单调递增版本：

```rust
pub struct Generation(u64);
```

它是整个失效机制的核心。

以下事件必须提升 generation：

- 重新加载 knowdb
- authority.sqlite 被重建
- provider 从 SQLite 切到 PostgreSQL
- PostgreSQL 连接配置变化
- 白名单变化
- 明确的 schema refresh

所有 cache key、线程本地状态、元数据缓存都必须带 generation。

### QueryRequest

内部统一查询模型，不再让 facade 直接依赖 `rusqlite::ToSql`：

```rust
pub struct QueryRequest {
    pub sql: String,
    pub params: Vec<QueryParam>,
    pub mode: QueryMode,
    pub cache_policy: CachePolicy,
}
```

```rust
pub enum QueryMode {
    Many,
    FirstRow,
    CipherTable { table: String },
}
```

```rust
pub enum QueryParam {
    Null { name: String },
    Bool { name: String, value: bool },
    Int { name: String, value: i64 },
    Float { name: String, value: f64 },
    Text { name: String, value: String },
}
```

第一阶段不需要把 `DataField` 的全部值类型都映射成数据库原生类型。对数据库参数来说，优先支持真正参与 bind 的类型即可。

### CachePolicy

缓存不应该对所有查询默认开启。建议显式建模：

```rust
pub enum CachePolicy {
    Bypass,
    UseGlobal { ttl: Option<std::time::Duration> },
    UseCallScope,
}
```

含义：

- `Bypass`
  - 不走结果缓存
- `UseGlobal`
  - 使用统一进程内缓存
- `UseCallScope`
  - 兼容历史 `cache_query` 的调用期局部缓存

这样可以兼容旧逻辑，也能逐步把热点 lookup 迁移到统一缓存。

## Provider 抽象

建议新增内部 trait：

```rust
pub trait KnowledgeProvider: Send + Sync {
    fn provider_kind(&self) -> ProviderKind;
    fn datasource_id(&self) -> &DatasourceId;
    fn execute(&self, req: &QueryRequest) -> KnowledgeResult<QueryResult>;
    fn generation_hint(&self) -> Option<Generation>;
}
```

其中：

- `provider_kind` 用于日志、指标、调试
- `datasource_id` 用于缓存隔离
- `execute` 是统一入口
- `generation_hint` 在第一阶段可选，后续可用于 provider 自报内部版本

SQLite 与 PostgreSQL 的差异应收敛在 provider 内部：

- SQLite provider 自己完成 `:name` 参数绑定
- PostgreSQL provider 自己把 `:name` 改写成 `$1/$2/...`
- metadata 获取逻辑也各自收敛

外层 runtime 不应该关心它们的方言差异。

## Cache 设计

### 1. 结果缓存

建议使用成熟并发缓存库 `moka::sync::Cache` 替代当前自定义 `FieldQueryCache` 作为全局缓存实现。

#### Key 结构

```rust
pub struct ResultCacheKey {
    pub datasource_id: DatasourceId,
    pub generation: Generation,
    pub query_hash: u64,
    pub params_hash: u64,
    pub mode: QueryModeTag,
}
```

要求：

- 必须包含 `datasource_id`
- 必须包含 `generation`
- 不能只按 SQL 文本缓存
- 参数需要标准化后再 hash

#### 标准化规则

- 保留参数名
- 保留参数值
- 不保留 `DataField` 的格式化元信息
- 对 `NULL`、布尔、数字、文本做稳定编码

#### 适用场景

- 字典 lookup
- 规则评估中的热点小查询
- 白名单表上的纯读查询

#### 默认策略

- 结果缓存默认关闭
- 由调用方或规则层显式启用
- `query_cipher` 可以默认启用全局缓存

#### 失效机制

- 主失效：`generation`
- 次失效：TTL
- 容量保护：`max_capacity`

### 2. 调用期缓存

当前 `FieldQueryCache` 不应直接删除，而应降级为“兼容层局部缓存”。

保留原因：

- OML/规则引擎里同一次转换会多次重复 lookup
- 局部 cache 仍然是很高 ROI 的优化
- 这层不需要并发

但它需要被重新定位：

- 不再承担全局一致性责任
- 不再被视为主缓存
- 只服务于单次计算内去重

建议后续改名为：

- `QueryLocalCache`

以避免和统一全局缓存混淆。

### 3. 列名 / 元数据缓存

当前 `COLNAME_CACHE` 应升级为统一 metadata cache，同样建议使用 `moka`。

#### Key 结构

```rust
pub struct MetadataCacheKey {
    pub datasource_id: DatasourceId,
    pub generation: Generation,
    pub query_hash: u64,
}
```

#### Value

```rust
pub struct QueryMetadata {
    pub columns: Arc<Vec<ColumnMeta>>,
}
```

```rust
pub struct ColumnMeta {
    pub name: String,
    pub logical_type: Option<String>,
}
```

第一阶段可以只填列名，先完全覆盖当前 `COLNAME_CACHE` 能力。

#### 策略

- 必须有容量上限
- 可选 TTL
- 与结果缓存一样受 `generation` 控制

## ThreadClonedMDB 的改造

这是整个方案里不能绕过的一点。

当前线程本地内存库是 provider 自己的“隐式大缓存”。如果不把它纳入 generation 体系，外层 cache 再成熟也无法保证 reload 后读到新数据。

建议将线程本地状态改成：

```rust
struct ThreadLocalState {
    generation: Generation,
    conn: Connection,
}
```

访问逻辑：

- 线程第一次访问：构建 `conn`
- 如果线程本地 generation 与 registry 当前 generation 不一致：
  - 丢弃旧 `conn`
  - 从新的 authority/provider 重建

这样才能保证：

- authority 重建后线程会自动切换到新快照
- provider 切换后线程不会继续读旧 SQLite 副本

## Facade 演进方案

### 对外兼容目标

以下 API 第一阶段继续保留：

- `query`
- `query_row`
- `query_named`
- `query_named_fields`
- `query_cipher`
- `cache_query`

但内部改造为：

- 统一构造 `QueryRequest`
- 交给 `QueryRuntime`
- 由 runtime 决定是否查全局缓存

### `cache_query` 的重新定义

建议把 `cache_query` 明确定位成兼容接口：

- 继续支持旧的调用期 cache
- 内部可选叠加全局缓存
- 长期目标是弱化它的存在感

建议执行顺序：

1. 查调用期 local cache
2. 若 miss 且允许全局缓存，则查 global result cache
3. 若仍 miss，则执行 provider 查询
4. 回填 global cache
5. 回填 local cache

这样既保留单次计算内去重，也让跨请求热点查询可复用。

## Reload 与运行时管理

建议新增一个运行时状态对象：

```rust
pub struct KnowledgeRuntime {
    pub registry: ArcSwap<ProviderHandle>,
    pub result_cache: moka::sync::Cache<ResultCacheKey, Arc<QueryResult>>,
    pub metadata_cache: moka::sync::Cache<MetadataCacheKey, Arc<QueryMetadata>>,
}
```

其中 `ProviderHandle` 包含：

- provider 实例
- datasource_id
- generation
- 白名单

reload 流程：

1. 构建新 provider
2. 生成新 `datasource_id` 或沿用原 id
3. `generation += 1`
4. 原子替换 registry 当前 handle
5. 不必同步清理所有 cache，依赖 generation 自然失效

必要时可异步触发：

- `invalidate_entries_if(...)`
- `run_pending_tasks()`

但这不是 correctness 的前提。

## 观测性

如果没有指标，这套方案后续很难评估。

建议新增以下指标与日志：

- `provider.kind`
- `provider.datasource_id`
- `provider.generation`
- `cache.result.hit`
- `cache.result.miss`
- `cache.metadata.hit`
- `cache.metadata.miss`
- `cache.local.hit`
- `cache.local.miss`
- `provider.reload.success`
- `provider.reload.failure`

至少应有 debug 日志能回答：

- 当前查询打到了哪个 provider
- 当前 generation 是多少
- 是 local cache 命中还是 global cache 命中
- 是否发生了线程本地副本重建

## 迁移步骤

建议拆成 5 个阶段，避免一次性重写。

### 阶段 1：引入运行时与 generation

- 新增 `KnowledgeRuntime`
- `PROVIDER` 从 `OnceLock` 升级为可替换运行时句柄
- provider handle 显式带 `datasource_id/generation`
- 现有 facade 改为经 runtime 分发

### 阶段 2：引入统一 QueryRequest

- 新增 `QueryRequest/QueryParam/QueryResult`
- SQLite / PostgreSQL provider 迁移到统一 `execute`
- `query_named` 从 `ToSql` 兼容层适配到 `QueryParam`

### 阶段 3：引入全局结果缓存与 metadata cache

- 接入 `moka`
- metadata cache 先替换 `COLNAME_CACHE`
- result cache 先只给 `query_cipher` 和显式声明可缓存的 query 使用

### 阶段 4：改造 ThreadClonedMDB

- 线程本地连接带 generation
- generation 变化时自动重建副本
- 统一接入 runtime 的 provider handle

### 阶段 5：缩减历史兼容层

- 弱化 `cache_query` 的主路径地位
- 将 `FieldQueryCache` 重命名为 `QueryLocalCache`
- 将更多热点 lookup 显式迁移到 global cache 策略

## 风险与取舍

### 风险 1：缓存污染

如果漏掉 `datasource_id` 或 `generation`，不同 provider 结果会串。

结论：

- 这两个字段必须进所有全局 cache key

### 风险 2：reload 后仍读旧数据

如果只替换 provider，不重建线程本地副本，SQLite 线程快照仍可能陈旧。

结论：

- `ThreadClonedMDB` 必须纳入 generation 体系

### 风险 3：默认缓存过度

外部数据库上的只读 SQL 并不一定是确定性的。

结论：

- 结果缓存默认关闭
- 对缓存行为采用显式策略

### 风险 4：接口一次性改太大

直接删除现有 facade API 会导致上游大面积改动。

结论：

- 保留 facade
- 先重构内部 runtime

## 建议依赖

建议新增：

```toml
moka = { version = "0.12", features = ["sync"] }
arc-swap = "1"
```

说明：

- `moka` 用于结果缓存与 metadata cache
- `arc-swap` 用于低成本原子切换当前 provider handle

## 最终判断

`wp-knowledge` 后续如果要稳定支持 SQLite 与外部数据库，必须把架构中心从“SQLite 查询工具集”转成“带版本语义的数据源查询运行时”。

这次重构的关键不是单纯替换缓存库，而是一起完成：

- provider 抽象去 SQLite 化
- runtime 接管 cache 策略
- generation 接管失效语义
- thread local snapshot 纳入统一版本管理

只有这四件事一起成立，`wp-knowledge` 才能在支持外部数据库的同时，真正拥有可维护的 cache 机制。
