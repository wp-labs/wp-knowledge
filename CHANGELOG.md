# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **MySQL Provider**: Add external MySQL query provider support with `kind = "mysql"`, connection-pool sizing via `[provider].pool_size`, and an ignored integration test driven by `WP_KDB_TEST_MYSQL_URL`
- **Tests/MySQL**: Add compose-managed local MySQL validation via `tests/test-mysql-provider.sh` and extend `tests/docker-compose.yml` with a reusable `mysql` service

### Changed
- **Postgres Provider**: Replace the single shared `postgres::Client` with an `r2d2` connection pool and allow configuring pool size via `[provider].pool_size`
- **Provider Config**: Remove `allowed_tables` from external provider configuration and keep runtime access on the general SQL query path only

### Removed
- **Facade/API**: Remove `facade::query_cipher(...)` and the related `DBQuery::query_cipher(...)` implementations from memory, thread-cloned, stub, and PostgreSQL providers
- **Documentation**: Drop stale `query_cipher` and `allowed_tables` references from provider docs and examples

### Fixed
- **Tests/Postgres**: Make the pooled-query concurrency test actually execute `pg_sleep(...)` as part of the query plan so the timing assertion validates real parallel checkout instead of an unreferenced CTE
