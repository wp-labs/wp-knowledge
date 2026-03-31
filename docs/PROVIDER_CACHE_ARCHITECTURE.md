# wp-knowledge Provider 与 Cache 重构方案

配套实施任务清单见：

- [PROVIDER_CACHE_TASKS.md](./PROVIDER_CACHE_TASKS.md)

## 背景

`wp-knowledge` 当前已经同时支持三类数据源：

- 本地 KnowDB 构建出的 SQLite authority 库
- 外部 PostgreSQL
- 外部 MySQL

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
  - 暴露现有 `query/query_row/query_named/cache_query`
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

### 术语与边界

当前实现里需要明确区分 3 类 cache。后续讨论中的 `cache`，如果不特别说明，不应混用这 3 个概念。

| 名称 | 文档统一称呼 | 当前主要实现 | 作用域 | 缓存内容 | 容量单位 | 默认容量 | 谁控制是否使用 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 结果缓存 | `result cache` | `KnowledgeRuntime.result_cache` | 进程级、provider 级 | 查询结果 `QueryResponse` | 条目数 | `1024 entries` | `CachePolicy::UseGlobal` |
| 调用期缓存 | `local cache` | `FieldQueryCache` / `QueryLocalCache` | 单次计算 / 单次调用链 | 查询结果 `RowData` | 条目数 | `100 entries` | 调用方是否传入并走 `cache_query` |
| 元数据缓存 | `metadata cache` | `COLNAME_CACHE` | 进程级、provider 级 | 列名 / metadata | 条目数 | `512 entries` | SQLite / PostgreSQL / MySQL 查询路径内部自动使用 |

必须注意：

- `result cache` 不是字节容量，而是“最多缓存多少条查询结果”
- `local cache` 不是全局共享缓存，而是调用方持有的局部对象
- `metadata cache` 不是业务数据缓存，它缓存的是列名 / schema 辅助信息

### 三类 cache 的职责定义

#### 1. result cache

这是“跨调用复用”的主缓存。

职责：

- 缓存热点查询结果
- 避免重复访问外部数据库或 SQLite provider
- 为跨请求、跨规则执行的重复 lookup 提供复用

边界：

- 它缓存的是“查询结果”
- 它不负责单次计算内的临时去重
- 它不缓存列名 metadata

当前实现特征：

- 作用域：进程级
- key：`datasource_id + generation + query_hash + params_hash + mode`
- 容量：`1024 entries`
- 启用方式：只有 `CachePolicy::UseGlobal` 才会使用
- 默认策略：普通 `query/query_row/query_fields` 默认不走它

#### 2. local cache

这是“单次计算内去重”的兼容层缓存。

职责：

- 消除一次规则执行 / 一次转换中的重复 lookup
- 降低同一批数据处理过程中的重复 query 开销
- 作为历史 `cache_query(...)` 语义的承载者继续存在

边界：

- 它不承担跨请求一致性责任
- 它不是主缓存
- 它只对持有这个 cache 对象的调用链可见

当前实现特征：

- 作用域：调用方局部
- key：调用方给出的 `DataField` 参数组合
- 容量：默认 `100 entries`，也可 `with_capacity(size)`
- 启用方式：调用方显式走 `cache_query/cache_query_fields`
- 与 `result cache` 的关系：`local miss` 后会继续走 `global result cache`

#### 3. metadata cache

这是“查询辅助信息缓存”，不是业务结果缓存。

职责：

- 缓存列名或 schema 辅助信息
- 降低重复 `prepare/describe` 或列信息提取的开销
- 给结果映射提供稳定的 metadata 复用

边界：

- 它不缓存查询返回的数据行
- 它不能替代 `result cache`
- 命中它并不意味着业务查询结果命中

当前实现特征：

- 作用域：进程级
- key：`datasource_id + generation + query_hash`
- 容量：`512 entries`
- 启用方式：SQLite / PostgreSQL / MySQL 内部查询路径自动使用
- 当前 value：主要是列名集合，仍偏轻量

### 三类 cache 的开关语义

这 3 类 cache 当前没有一个统一的总开关，必须分别理解：

- `result cache`
  - 代码路径开关
  - 只有 `CachePolicy::UseGlobal` 才启用
- `local cache`
  - 调用方式开关
  - 只有 `cache_query(...)` 或显式传入 `FieldQueryCache` 才启用
- `metadata cache`
  - 当前默认始终启用
  - 没有单独配置项

因此，当前“开 cache”并不是一个布尔配置，而是“调用路径是否进入对应 cache 层”。

### 当前配置契约

当前已经配置化的只有 `result cache`，并且配置必须遵守 `knowdb.toml -> EnvTomlLoad -> KnowDbConf` 的现有加载契约。

也就是说：

- 配置位置在 `knowdb.toml`
- 由 `parse_knowdb_conf(...)` 一次性加载
- 由 `init_thread_cloned_from_knowdb(...)` 在 provider 初始化成功后应用到 runtime
- 不额外引入独立的环境变量解析或旁路配置源

当前支持的配置如下：

```toml
[cache]
enabled = true
capacity = 1024
ttl_ms = 30000
```

语义：

- `enabled`
  - 控制 `result cache` 总开关
  - `false` 时，所有 `CachePolicy::UseGlobal` 都退化为 `Bypass`
- `capacity`
  - 单位是条目数 `entries`
  - 表示最多缓存多少条查询结果
- `ttl_ms`
  - 单位是毫秒
  - 表示 result cache entry 的最大存活时间

明确不在这组配置里的内容：

- `local cache`
  - 仍由调用方持有 `FieldQueryCache` 时自行决定
- `metadata cache`
  - 当前仍为内部固定配置，不受 `[cache]` 影响

因此，当前 `[cache]` 的准确含义应理解为：

- `result cache config`
- 不是整个 `wp-knowledge` 所有缓存层的总配置

### 三类 cache 的失效语义

#### 共同点

- 主失效机制都是 `generation`
- provider reload / knowdb reload / provider replace 后，`generation` 会递增
- 因此新查询不会再命中旧 generation 的 key

#### 差异点

`result cache`：

- 旧 entry 不会立刻物理删除
- 但因为 key 带 `generation`，逻辑上已经失效
- 后续由 LRU 淘汰

`local cache`：

- 在 `prepare_generation(...)` 时发现 generation 变化，会直接 `reset()`
- 也就是“逻辑失效 + 物理清空”

`metadata cache`：

- 与 `result cache` 类似，旧 key 因 generation 变化而不再命中
- 后续由容量淘汰

### 源数据变化时，哪一层会自动失效

这是最容易混淆的一点，必须单独说明。

当前只有“provider generation 变化”会触发可靠失效。

也就是说：

- 如果发生 `reload`
- 或重新安装 provider
- 或 authority 重建

那么：

- `result cache` 会因为新 generation 自然失效
- `local cache` 会直接清空
- `metadata cache` 会因为新 generation 自然失效

但如果是“外部数据库里的源表数据被别人改了”，而 `wp-knowledge` 本身没有 reload：

- `result cache` 不会自动失效
- `local cache` 也不会自动知道外部源数据变化
- `metadata cache` 通常也不会变化，除非 schema 本身变化且伴随 reload

所以当前一致性边界必须定义为：

- cache 一致性跟随 `provider generation`
- 不跟随外部数据库实时数据变化

换句话说，当前没有这些机制：

- TTL 自动过期
- CDC 驱动失效
- 表版本号探测
- 后台数据变化自动 invalidate

后续如果要配置化，建议也按这 3 层分别配置，而不是只给一个笼统的 `cache=true/false`。

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

## 性能验证结果

为避免只停留在架构层面，当前仓库已经补了 3 组可重复执行的 cache 性能测试：

- 内存 provider 对比：`tests/test-cache-perf.sh`
- PostgreSQL 真 provider 对比：`tests/test-postgres-provider.sh`
- MySQL 真 provider 对比：`tests/test-mysql-provider.sh`

对比口径统一为 3 条路径：

- `bypass`
  - 完全不使用结果缓存
- `global_cache`
  - 使用 runtime 级 result cache
- `local_cache`
  - 先查调用期 local cache，miss 时再落到 runtime result cache

### 默认样本参数

默认使用以下参数：

- `WP_KDB_PERF_ROWS=10000`
- `WP_KDB_PERF_OPS=10000`
- `WP_KDB_PERF_HOTSET=128`

含义：

- 表中准备 `10000` 条数据
- 单轮执行 `10000` 次 lookup
- 实际热点 key 集合为 `128`，故缓存命中率很高，适合评估 lookup 场景收益

### 实测结果

以下结果来自当前仓库实现、当前开发机、本地 Docker provider 集成测试实跑输出。

#### 1. 内存 provider

运行入口：

```bash
./tests/test-cache-perf.sh
```

一次实测结果：

| Scenario | Elapsed | QPS | Result Cache | Local Cache |
| --- | ---: | ---: | --- | --- |
| `bypass` | `1204 ms` | `99,658` | `hit=0 miss=0` | `hit=0 miss=0` |
| `global_cache` | `116 ms` | `1,034,157` | `hit=119872 miss=128` | `hit=0 miss=0` |
| `local_cache` | `71 ms` | `1,686,998` | `hit=0 miss=128` | `hit=119872 miss=128` |

加速比：

- `global_cache vs bypass`: `10.38x`
- `local_cache vs bypass`: `16.93x`

说明：

- 内存 provider 本身已经很快，因此 cache 主要减少的是重复 SQL 执行与重复 metadata/row 组装开销
- `local_cache` 略快于 `global_cache`，符合“同次计算内重复 lookup”场景预期

#### 2. PostgreSQL provider

运行入口：

```bash
./tests/test-postgres-provider.sh
```

一次实测结果：

| Scenario | Elapsed | QPS | Result Cache | Local Cache |
| --- | ---: | ---: | --- | --- |
| `bypass` | `5201 ms` | `1,923` | `hit=0 miss=0` | `hit=0 miss=0` |
| `global_cache` | `79 ms` | `125,212` | `hit=9872 miss=128` | `hit=0 miss=0` |
| `local_cache` | `73 ms` | `136,901` | `hit=0 miss=128` | `hit=9872 miss=128` |

加速比：

- `global_cache vs bypass`: `65.13x`
- `local_cache vs bypass`: `71.21x`

说明：

- 外部数据库场景下，cache 的收益远高于内存 provider
- 主因不是 SQL 本身复杂，而是绕开了驱动绑定、连接 checkout、网络往返与结果反序列化
- 对热点小查询，runtime result cache 已经能显著削平外部库成本

#### 3. MySQL provider

运行入口：

```bash
./tests/test-mysql-provider.sh
```

一次实测结果：

| Scenario | Elapsed | QPS | Result Cache | Local Cache |
| --- | ---: | ---: | --- | --- |
| `bypass` | `5757 ms` | `1,737` | `hit=0 miss=0` | `hit=0 miss=0` |
| `global_cache` | `82 ms` | `121,016` | `hit=9872 miss=128` | `hit=0 miss=0` |
| `local_cache` | `73 ms` | `136,587` | `hit=0 miss=128` | `hit=9872 miss=128` |

加速比：

- `global_cache vs bypass`: `69.67x`
- `local_cache vs bypass`: `78.63x`

说明：

- MySQL 与 PostgreSQL 的结论一致：在热点 lookup 场景下，cache 带来的收益是数量级级别的
- `local_cache` 继续略优于 `global_cache`，说明对单次规则执行内的重复 key，调用期缓存仍然有意义

### 结果解读

从这 3 组结果可以得到几个明确结论：

- `cache` 不是“可能有帮助”，而是对热点 lookup 场景有决定性价值
- 对外部 provider，单纯依赖连接池并不足够，真正的大头仍是重复查询路径本身
- `global_cache` 已经能覆盖跨调用复用价值
- `local_cache` 仍然值得保留，尤其适合一次转换内大量重复 key 的规则
- 因为当前实现已经把 `generation` 编入 runtime cache 失效语义，所以收益并不是以 correctness 为代价换来的

### 注意事项

- 这些数据是热点分布 workload，不代表“所有 SQL 都应该默认缓存”
- 对非确定性查询、低复用查询、结果集大的查询，cache 收益会下降，甚至可能不划算
- 当前默认参数适合日常回归；若要更接近生产场景，应按真实热点分布重跑
- 当前文档记录的是“现有实现的实测值”，不是长期性能承诺

### 建议的工程结论

基于当前实测结果，建议保持以下策略：

- 结果缓存继续默认关闭，只对明确热点查询显式启用
- 兼容层 `cache_query` 保留，作为 OML/规则执行内 local cache 的主要入口
- 对 PostgreSQL/MySQL provider，优先把高频 lookup 类 SQL 显式迁移到 `UseGlobal` 或 `cache_query`
- 后续若要继续优化，应优先做 cache 策略收敛与配置化，而不是先去微调驱动层 SQL 开销

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
- result cache 先只给显式声明可缓存的 query 使用

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
