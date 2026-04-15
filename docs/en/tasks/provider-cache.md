# Provider / Cache Refactor Tasks

[中文](../../zh/tasks/provider-cache.md) | English

## Context

`wp-knowledge` already supports:

- local SQLite authority / thread-cloned provider
- external PostgreSQL provider
- external MySQL provider

But the provider and cache architecture still mostly follows older SQLite-driven implementation history:

- `facade` exposes `QueryFacade`, but the compatibility layer still carries `rusqlite::ToSql` assumptions
- result caching still relies heavily on caller-provided `FieldQueryCache`
- `COLNAME_CACHE` is still a process-wide, unbounded, generation-unaware `HashMap`
- `ThreadClonedMDB` does not give thread-local snapshots a unified versioning contract
- provider lifecycle still lacks a formal reload / replace mechanism

Related architecture notes:

- [provider-cache.md](../architecture/provider-cache.md)

The current state is good enough to keep expanding database backends, but not enough for long-term evolution with multiple providers, controlled caching, and reload support.

## Execution rule

Because `wp-knowledge` keeps evolving, every task below must start with a fresh read of the current code instead of assuming the document is still fully current.

Execution requirements:

- read the current implementation first and confirm which capabilities already landed
- produce a gap analysis between the document's target state and the real code
- if code already covers part of the task, shrink the task before implementation
- if the implementation drifted away from the architecture document, update the task split and acceptance criteria first
- evaluate acceptance against the current code baseline, not against the older state when this document was first written

Suggested work pattern:

1. evaluate current code
2. mark done / partially done / not started
3. shrink the current task scope
4. then implement, test, and update docs
5. perform a self-review after completion
6. fix review findings before closing

## Completion rule

A task is not complete immediately after implementation. It must be followed by a review pass, and review findings must be addressed.

Execution requirements:

- review the change from a code-review perspective
- prioritize behavior regressions, edge cases, naming issues, test gaps, and missing docs
- if review finds issues, fix them first and verify again
- only close the task when review leaves no open problems

The minimum closure loop should be:

1. implement
2. test / verify
3. review
4. fix review findings
5. verify again
6. update the necessary docs

## Current status

Based on the code baseline as of 2026-03-25:

- Completed:
  - `KnowledgeRuntime` already exists; providers are installable as replaceable handles and explicitly carry `datasource_id + generation`
  - facade dispatch already goes through the runtime, and `query/query_row/query_fields/cache_query` use the unified execution path
  - a unified `QueryRequest / QueryParam / QueryResponse / CachePolicy` model already exists
  - a bounded global result cache already exists and its key includes `datasource_id + generation + query_hash + params_hash + mode`
  - `FieldQueryCache` already has generation-aware invalidation and also exposes the `QueryLocalCache` alias
  - `ThreadClonedMDB` already rebuilds thread-local snapshots when generation changes
  - SQLite metadata cache already invalidates naturally via `datasource_id + generation + query_hash` and uses a bounded `LruCache`

- Partially completed:
  - reload / observability already has baseline logs, runtime snapshots, cache/reload counters, an installable telemetry hook, and fallback behavior that keeps the old provider on reload failure; what remains is host-side exporter/adapter integration and a more unified diagnostics surface
  - the compatibility layer still keeps `query_named(...)` and `cache_query(...)`, while provider-neutral API migration and retirement strategy are still in progress

- Not started or not fully expanded:
  - host-side metrics / telemetry adapters such as Prometheus or `wp-stats`
  - deprecation strategy for the compatibility surface

## Priority 1

### 1. Introduce replaceable runtime shell

Current:

- facade still holds a global provider directly
- provider lifecycle lacks formal replace / reload semantics

Tasks:

- introduce `KnowledgeRuntime`
- move the global provider into a runtime-owned provider handle
- make the provider handle explicitly carry `datasource_id` and `generation`
- route facade dispatch through the runtime

Acceptance:

- provider no longer depends on one-shot `OnceLock`
- the internal structure is ready for reload / replace

### 2. Define provider-neutral request / result model

Current:

- SQLite / PostgreSQL / MySQL providers all bind parameters differently
- facade compatibility APIs still expose SQLite-shaped parameter interfaces

Tasks:

- introduce `QueryRequest`, `QueryParam`, and `QueryResult`
- add internal `KnowledgeProvider::execute(...)`
- migrate SQLite / PostgreSQL / MySQL providers to the unified request model
- keep facade compatibility APIs, but stop depending on `rusqlite::ToSql` internally

Acceptance:

- provider differences are contained inside provider implementations
- the runtime operates only on the shared request model

## Priority 2

### 3. Replace `COLNAME_CACHE` with bounded metadata cache

Current:

- `COLNAME_CACHE` keys only on SQL text
- it does not distinguish provider or generation
- it has no capacity limit

Tasks:

- introduce a unified metadata cache
- key it with at least `datasource_id + generation + query_hash`
- let the first version cover the current column-name cache behavior
- add capacity bounds and optionally TTL

Acceptance:

- no more process-wide unbounded `HashMap` for column names
- metadata cache naturally invalidates across provider / generation changes

### 4. Introduce global result cache with explicit policy

Current:

- result caching mainly depends on caller-provided `FieldQueryCache`
- cache keys do not fully encode provider identity and version

Tasks:

- introduce a unified global result cache
- define `CachePolicy` with at least `Bypass / UseGlobal / UseCallScope`
- include `datasource_id + generation + query_hash + params_hash + mode` in cache keys
- first enable it only for queries that explicitly opt in

Acceptance:

- global result cache never mixes data across provider / generation boundaries
- default query behavior remains conservative and does not over-cache implicitly

## Priority 3

### 5. Re-scope local cache as compatibility layer

Current:

- `FieldQueryCache` looks like a primary cache by name, but in practice it only fits per-call deduplication

Tasks:

- reposition `FieldQueryCache` as local cache
- evaluate renaming it to `QueryLocalCache`
- define `cache_query` execution order as local cache -> global cache -> provider query
- keep existing OML / rule-engine paths working

Acceptance:

- local cache and global cache have clear, separate semantics
- `cache_query` no longer carries responsibility for global consistency

### 6. Make `ThreadClonedMDB` generation-aware

Current:

- a thread keeps its local snapshot after first access
- provider reload or authority rebuild does not automatically invalidate the old snapshot

Tasks:

- add `generation` to thread-local state
- drop and rebuild the snapshot when generation changes
- bring the thread-local snapshot under the runtime provider-handle model

Acceptance:

- threads automatically switch to the new snapshot after authority rebuild
- provider switches do not keep reading the old SQLite snapshot

## Priority 4

### 7. Define reload and observability contract

Current:

- provider switching and cache invalidation still lack a unified observability surface

Tasks:

- define the runtime reload flow
- add key logs and metrics for provider / cache / generation
- define failure behavior and rollback strategy for reload

Acceptance:

- operators can answer which provider, which cache layer, and which generation served the current query
- reload success and failure have clear diagnostics

### 8. Clean up compatibility surface

Current:

- facade compatibility APIs still carry historical baggage

Tasks:

- define the long-term position of `query_named(...)` and `cache_query(...)`
- evaluate whether SQLite-shaped compatibility APIs should be marked `deprecated`
- continue migrating in-repo code to provider-neutral entry points

Acceptance:

- new code defaults to provider-neutral APIs
- the remaining scope and retirement path of the compatibility layer are explicit

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

- In phase 1, do not aim to delete every old API at once. Establish runtime and generation semantics first.
- PostgreSQL / MySQL support does not mean the architecture problem is solved; it only makes the refactor more urgent.
- `ThreadClonedMDB` is on the correctness-critical path and should not be deferred to the very end.
