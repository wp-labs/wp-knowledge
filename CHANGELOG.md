# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- No unreleased changes yet.  
  当前没有未发布变更。

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
