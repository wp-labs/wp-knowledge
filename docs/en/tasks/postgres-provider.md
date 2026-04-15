# PostgreSQL Provider Tasks

[中文](../../zh/tasks/postgres-provider.md) | English

## Context

`wp-knowledge` already has baseline support for an external PostgreSQL provider:

- `knowdb.toml` supports `[provider] kind = "postgres"`
- `facade::query_fields(...)` and `cache_query_fields(...)` are the recommended provider-neutral entry points
- `facade::query_named(...)` and `cache_query(...)` are still kept as wrappers for the older SQLite-shaped parameter interface
- baseline PostgreSQL queries, named-parameter rewriting, and manual verification scripts are already available

The current state is good enough to validate the main flow, but not yet strong enough to be treated as long-term, production-grade PostgreSQL support.

## Priority 1

### 1. Replace single-client provider with a connection pool

Current:

- `src/postgres.rs` uses `Mutex<Client>`, so all queries are serialized through one client

Tasks:

- evaluate and integrate a PostgreSQL connection pool
- move from a single client to pooled connection checkout
- define clear behavior for connect failures, disconnects, and retries

Acceptance:

- concurrent queries no longer depend on one serialized client
- baseline tests and PostgreSQL integration tests all pass

### 2. Add provider startup validation

Current:

- provider initialization only checks whether a connection can be established
- missing tables, permission issues, and schema drift are only exposed when real queries run

Tasks:

- add basic startup self-checks
- emit clear errors that distinguish connection problems, permission problems, and schema problems

Acceptance:

- wrong database, wrong schema, or wrong permissions fail during initialization

## Priority 2

### 3. Define SQL/UDF compatibility boundary

Current:

- PostgreSQL currently supports baseline SQL querying and named-parameter rewriting
- the SQLite UDF set has not been migrated to PostgreSQL

Tasks:

- inventory the common SQLite UDF dependencies used by `wp-knowledge` and `wp-oml`
- define which SQL can move directly to PostgreSQL
- define which SQL must be rewritten or requires user-provided PostgreSQL functions
- document the boundary in docs and examples

Acceptance:

- external users can see a clear list of SQL capabilities that work directly versus those that require adaptation

### 4. Expand PostgreSQL type coverage

Current:

- `src/postgres.rs` supports only part of the result-column type space
- many `DataField` values still degrade to text when bound as parameters

Tasks:

- extend PostgreSQL result-type mappings
- evaluate and add more appropriate parameter binding behavior
- return more specific errors for unsupported types

Acceptance:

- common numeric, temporal, JSON, and network-related types have an explicit support strategy

## Priority 3

### 5. Complete provider-neutral API migration

Current:

- new code can use `query_fields(...)` and `cache_query_fields(...)`
- compatibility APIs still expose `rusqlite::ToSql`

Tasks:

- continue migrating in-repo call sites to the provider-neutral APIs
- evaluate whether compatibility APIs should be marked `deprecated`
- define the long-term keep/remove strategy for `query_named(...)` and `cache_query(...)`

Acceptance:

- new code paths no longer depend on SQLite-only types by default
- the role of the compatibility layer is clear

### 6. Improve automated verification

Current:

- `tests/postgres_provider.rs` requires explicit environment preparation
- `tests/postgres_testcontainers.rs` is currently `ignored`
- `tests/test-postgres-provider.sh` is available for manual verification with local Docker Desktop

Tasks:

- design an automated PostgreSQL validation path for CI
- decide whether to keep Compose or adopt `testcontainers`
- make sure failures provide enough diagnostics for connections, containers, and SQL

Acceptance:

- PostgreSQL provider behavior can be verified reliably in normal development and CI workflows

## Priority 4

### 7. Add end-to-end example in a consuming repo

Current:

- `wp-knowledge` itself already supports a PostgreSQL provider
- but `wp-motor` / `wp-oml` still lacks a complete, reproducible PostgreSQL example chain

Tasks:

- add a PostgreSQL knowdb configuration example in a consuming repository
- validate the `wp-oml` call chain with a real schema and queries
- document the steps so team members can reuse them directly

Acceptance:

- a consuming project can verify the full PostgreSQL provider integration path end to end

## Suggested execution order

1. connection pool
2. startup validation
3. SQL/UDF capability boundary docs
4. type coverage expansion
5. automated verification
6. end-to-end example in a consuming repo
7. compatibility API deprecation strategy
