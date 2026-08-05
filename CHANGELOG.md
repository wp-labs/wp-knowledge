# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.15.1 Unreleased]

### Added
- **Intranet network knowledge (`intranet_nets`)**: 新增 `intranet_nets` 模块，统一管理"哪些 IP 段属于内网"的知识。配置直接放在 `knowdb.toml` 的 `[intranet_nets]` 节（随 knowdb.toml 解析自动注入），内置默认网段（RFC1918 + IPv4/IPv6 loopback + IPv6 ULA）+ 外部配置合并（`add` / `replace`）。提供 `is_intranet(ip)` 查询接口（IPv4/IPv6 分桶扫描；IPv4-mapped IPv6 按 IPv4 判定）、`generate_default_intranet_nets_config`（项目初始化生成 knowdb.toml 节）、`check_intranet_nets_config`（从 knowdb.toml 校验节，供 wproj check）。
  中文：新增 `intranet_nets` 模块，统一管理"哪些 IP 段属于内网"的知识。配置直接放在 `knowdb.toml` 的 `[intranet_nets]` 节（随 knowdb.toml 解析自动注入），内置默认网段（RFC1918 + IPv4/IPv6 loopback + IPv6 ULA）+ 外部配置合并（`add` / `replace`）。提供 `is_intranet(ip)` 查询接口（IPv4/IPv6 分桶扫描；IPv4-mapped IPv6 按 IPv4 判定）、`generate_default_intranet_nets_config`（项目初始化生成 knowdb.toml 节）、`check_intranet_nets_config`（从 knowdb.toml 校验节，供 wproj check）。

### Changed
- **Merge v0.14.2 + v0.15.0**: 合并两条已发布版本线（`[fun]` 命名查询 + `Value::BigUint` 任意精度参数），合流版本 `0.15.1`。
- **Dependencies**: 新增 `ipnet`（IP 网段集合）、`toml`（配置解析）、`once_cell`（全局 Lazy）依赖。

## [0.15.0]

### ⚠️ BREAKING CHANGES

- **Dependency `wp-model-core` 0.8 → 0.9**: upstream added `Value::BigUint` / `DataType::BigInt` variants; version bumped to 0.15.0.  
  依赖 `wp-model-core` 0.8 → 0.9（上游新增 `Value::BigUint` / `DataType::BigInt` 变体），版本升至 0.15.0。

### Added

- **Arbitrary-precision integer parameters (`Value::BigUint`)**: IPv4/IPv6 统一数值键等超出 `i64` 范围的整数可作为 SQL 参数，无精度损失。  
  新增任意精度整数参数（`Value::BigUint`）支持，用于 IPv4/IPv6 统一数值键等超出 `i64` 范围的整数场景：
  - **PostgreSQL**: `Value::BigUint` 绑定为 `BigDecimal`，编码为 `numeric` 参数。  
    PostgreSQL：绑定为 `BigDecimal`（`numeric` 编码）。
  - **MySQL**: 绑定为 `BigDecimal`，编码为 `DECIMAL` 参数。  
    MySQL：绑定为 `BigDecimal`（`DECIMAL` 编码）。
  - **SQLite**: 以十进制文本绑定，SQLite 数值比较按 affinity 自动转换。  
    SQLite：以十进制文本绑定，数值比较按 affinity 自动转换。
  - **缓存/参数序列化**: `stable_field_params_hash` 与 `fields_to_params` 支持 `Value::BigUint`（十进制文本）。  
    缓存键 hash 与参数序列化支持 `Value::BigUint`（十进制文本）。

### Dependencies

- `wp-model-core`: `0.8` → `0.9`.  
  `wp-model-core`：`0.8` → `0.9`。
- Add `num-bigint = "0.4"` for `Value::BigUint` interop.  
  新增 `num-bigint = "0.4"` 依赖（与 `Value::BigUint` 互操作）。

### Tests

- SQLite `ToSql` BigUint binding round-trip（十进制文本，含 IPv6 统一键 `382824323044708348099391746388336347272`）。
- `fields_to_params` BigUint → `QueryValue::Text` 与参数 hash 稳定性。

### Fixed

- Clippy `-D warnings`: remove redundant reference in `loader.rs` `format!`.  
  Clippy 修复：`loader.rs` `format!` 冗余引用。

## [0.14.2]

### Added
- **命名查询 `[fun]`**: knowdb.toml 新增 `[fun.<name>]` 配置段，通过 `call` 字段将 Redis 命令封装为命名查询。wfusion 只需传 service name + arg，不感知底层是 Bloom、Hash 还是 Set。
- **`external_exists` / `external_value` API**: 两个公开函数，分别对应 `bool` 和 `Option<String>` 返回值，调用时自动校验 call 类型。
- **Per-service 缓存控制**: `[fun.<name>]` 支持 `cache = false` 关闭缓存、`ttl_ms` 设置独立过期时间。

### Changed
- **缓存配置简化**: 移除 `[[cache.redis_key]]`，per-key 缓存控制统一到 `[fun.<name>]` 的 `cache` / `ttl_ms` 字段。
- **缓存 TTL**: 新增 `CachedRedisEntry { cached_at, ttl_ms }`，支持 per-entry TTL 过期。ttl_ms=0 时仅用 generation 失效。

## [0.14.1]

### Added
- **Redis 类型化 API**: `redis_bf_exists` / `redis_hget` / `redis_get` / `redis_set_exists` / `redis_bf_add` / `redis_bf_create` 六个函数，返回值类型化（`bool`、`Option<String>`、`Vec<bool>`）。
- **Redis 结果缓存**: 四个读函数自动使用进程内 LRU 缓存，复用 `[cache]` 配置，generation 机制自然失效。
- **缓存单元测试**: CI-safe 缓存行为测试（hit/miss、global disable、generation 隔离）。

### Changed
- **Redis API 简化**: 移除 name 参数、魔法命令字符串、通用 `redis_exec`。Provider 名称由内部管理。
- **Provider Config 简化**: knowdb.toml 的 `[provider]` 拆分为 `[provider.sqldb]` 和 `[provider.redis]`，旧平铺格式已移除。

## [0.14.0]

### Added
- **Redis Provider**: 新增 Redis 外部数据源支持，适用于弱口令 Bloom filter、威胁情报 IP 查表、白名单排除等高速查表场景。支持 `GET`、`HGET`、`BF.EXISTS`、`SISMEMBER`（P0）以及 `BF.MADD`、`BF.RESERVE`（P1）六种命令。
- **Redis Config**: knowdb.toml 新增 `[provider.redis]` 配置段，支持 `connection_uri`、`pool_size`、`connect_timeout_ms`（默认 3000）、`command_timeout_ms`（默认 100）。同一 Redis 实例的多个 provider 自动共享连接。

## [0.13.0]

### Changed
- Upgrade the error stack to `orion-error 0.8`.  
  升级错误处理栈到 `orion-error 0.8`。
- Rename `Reason` to `KnowReason`; keep `Reason` as deprecated alias.  
  将 `Reason` 重命名为 `KnowReason`，保留 deprecated 别名。

## [0.12.0]

### Changed
- **Error handling**: Replace `wp_error::KnowledgeReason` with local `Reason` type using `#[derive(OrionError)]` pattern, providing stable error codes (`biz.not_data`, transparent `Uvs`) via `StructError<Reason>`.  
  将 `wp_error::KnowledgeReason` 替换为本地 `Reason` 类型，使用 `#[derive(OrionError)]` 模式，通过 `StructError<Reason>` 提供稳定的错误码。
- **Remove `AnyResult`**: Remove the `AnyResult` type alias; convert all production and test code from `anyhow::Result` to `KnowledgeResult` (`StructError`-based), keeping `with_conn` as the intentional anyhow bridge.  
  删除 `AnyResult` 类型别名，将所有生产与测试代码从 `anyhow::Result` 改造为 `KnowledgeResult`（基于 `StructError`），保留 `with_conn` 作为有意的 anyhow 桥接层。
- **orion-error 0.7 migration**: Upgrade orion-error 0.6 → 0.7; migrate error-building callsites to new API — `ErrorOweBase::owe()` replaces deprecated `ErrorOwe::owe_res/owe_conf/owe_rule`, `OperationContext::doing()` replaces `.want()`, `ToStructError` from `conversion::`, `ContextRecord` from `runtime::`.  
  升级 orion-error 0.6 → 0.7，迁移错误构建调用点到新 API。

### Removed
- **`wp-error` dependency**: Remove `wp-error = "0.9"` dependency; add `derive_more = "2.0"` for `From` derive support on `Reason`.  
  删除 `wp-error = "0.9"` 依赖，增加 `derive_more = "2.0"` 以支持 `Reason` 的 `From` derive。

## [0.11.6]

### Added
- Add dedicated correctness/perf provider scripts and GitHub Actions workflows for MySQL and PostgreSQL validation.  
  增加面向 MySQL 与 PostgreSQL 的 correctness/perf 独立脚本，以及对应的 GitHub Actions 工作流。

### Changed
- Refactor provider runtime and shared helpers to reduce duplicated MySQL/PostgreSQL logic while keeping existing facade behavior stable.  
  重构 provider runtime 与公共辅助逻辑，减少 MySQL/PostgreSQL 间重复实现，同时保持现有 facade 行为稳定。
- Extend provider pool configuration with `min_connections`, `acquire_timeout_ms`, `idle_timeout_ms`, and `max_lifetime_ms`.  
  扩展 provider 连接池配置，增加 `min_connections`、`acquire_timeout_ms`、`idle_timeout_ms`、`max_lifetime_ms`。

### Fixed
- Fix binary/text decoding semantics and expand type compatibility coverage for MySQL/PostgreSQL, including `BYTEA`, `ENUM`, `SET`, `UUID`, `INET`, and `CIDR`.  
  修复 MySQL/PostgreSQL 的二进制与文本解码语义，并扩展类型兼容覆盖，包括 `BYTEA`、`ENUM`、`SET`、`UUID`、`INET`、`CIDR`。

## [0.11.5]

### Changed
- Replace PostgreSQL and MySQL provider internals with `sqlx` pools, while keeping existing facade APIs and named-parameter behavior.  
  将 PostgreSQL 与 MySQL Provider 内部实现替换为 `sqlx` 连接池，同时保持现有 facade API 与命名参数行为不变。

### Fixed
- Fix MySQL/PostgreSQL type decoding compatibility and reconnect regression coverage.  
  修复 MySQL/PostgreSQL 类型解码兼容性问题，并补齐重连回归覆盖。

## [0.11.1]

### Added
- Add async provider query APIs and async/runtime regression coverage for SQLite, PostgreSQL, and MySQL providers.  
  增加 async Provider 查询接口，以及面向 SQLite、PostgreSQL、MySQL Provider 的 async/runtime 回归测试覆盖。
- Add async provider performance documentation and benchmark notes.  
  增加 async Provider 性能测试与结论文档。

### Changed
- Switch PostgreSQL and MySQL providers to Tokio-based async execution with pooled runtime-managed query paths.  
  将 PostgreSQL 与 MySQL Provider 切换为基于 Tokio 的异步执行路径，并统一到带连接池的 runtime 管理查询模型。
- Extend provider runtime with async execution entry points while keeping generation-aware reload and cache semantics aligned across sync and async paths.  
  为 provider runtime 增加 async 执行入口，同时保持 sync/async 两条路径上的 generation 感知 reload 与 cache 语义一致。

### Fixed
- Fix metadata cache scoping so SQLite, PostgreSQL, and MySQL queries keep datasource/generation isolation across reloads.  
  修复 metadata cache 作用域问题，确保 SQLite、PostgreSQL、MySQL 在 reload 前后保持 datasource/generation 隔离。
- Fix SQLite async bridge so queued async queries keep the captured provider handle instead of jumping to a newer provider after reload.  
  修复 SQLite async bridge，避免排队中的异步查询在 reload 后跳转到更新后的 provider。
- Fix PostgreSQL named-parameter rewriting so casts, comments, and dollar-quoted bodies are not misparsed as placeholders.  
  修复 PostgreSQL 命名参数重写逻辑，避免将 cast、注释和 dollar-quote 体误判为占位符。
- Fix first-row query paths to avoid materializing full result sets when only one row is required.  
  修复 first-row 查询路径，避免在只需要一行时先物化整个结果集。

## [0.11.0]

Added since `v0.10.4`.  
自 `v0.10.4` 以来新增内容。

### Added
- Add external MySQL provider support, including local Compose validation scripts.  
  增加外部 MySQL Provider 支持，并补充本地 Compose 验证脚本。
- Add `[cache] enabled/capacity/ttl_ms` in `knowdb.toml` to control runtime result cache.  
  在 `knowdb.toml` 中增加 `[cache] enabled/capacity/ttl_ms`，用于控制 runtime result cache。
- Add runtime snapshot and installable telemetry hooks for provider, cache, query, and reload diagnostics.  
  增加 runtime snapshot 与可安装的 telemetry hook，用于观测 provider、cache、query 与 reload 状态。
- Add cache performance scripts and provider-level cache perf coverage for SQLite, PostgreSQL, and MySQL.  
  增加 SQLite、PostgreSQL、MySQL 的 cache 性能脚本与 provider 级性能验证。
- Add generation-aware provider runtime behavior for reload, result cache, local cache, and metadata cache.  
  增加带 generation 感知的 provider runtime 语义，覆盖 reload、result cache、local cache 与 metadata cache。
- Add metadata cache and cache telemetry support for PostgreSQL and MySQL query paths, including empty-result queries.  
  为 PostgreSQL / MySQL 查询路径增加 metadata cache 与对应 cache telemetry，并覆盖空结果查询场景。
- Add documentation for cache architecture, configuration, and invalidation behavior.  
  增加 cache 架构、配置项与失效语义相关文档。

### Changed
- Switch PostgreSQL provider to pooled connections and support `[provider].pool_size`.  
  PostgreSQL Provider 改为连接池实现，并支持 `[provider].pool_size` 配置。

### Removed
- Remove `query_cipher` and related stale code and documentation references.  
  删除 `query_cipher` 及相关过期代码与文档引用。
- Remove `allowed_tables` from external provider configuration.  
  从外部 provider 配置中删除 `allowed_tables`。

### Fixed
- Fix local cache scoping so different SQL statements do not accidentally share the same cache entry.  
  修复 local cache 作用域问题，避免不同 SQL 意外共享同一条缓存记录。
- Fix result-cache config application timing so failed provider reloads do not leak new cache settings onto the previous provider.  
  修复 result cache 配置应用时机，避免 provider reload 失败后污染旧 provider 的 cache 配置。
- Fix PostgreSQL / MySQL provider tests and naming after the API cleanup.  
  修复 PostgreSQL / MySQL provider 相关测试与命名问题。
