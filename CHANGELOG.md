# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
