# wp-knowledge Provider and Cache Refactor Plan

[中文](../../zh/architecture/provider-cache.md) | English

Companion task list:

- [provider-cache.md](../tasks/provider-cache.md)

## Background

`wp-knowledge` already supports three data-source families:

- local SQLite authority built from KnowDB assets
- external PostgreSQL
- external MySQL

But the implementation still carries clear SQLite-era assumptions:

- `facade` exposes a `QueryFacade`, but some APIs are still shaped around `rusqlite::ToSql`
- query caching still relies heavily on caller-owned `FieldQueryCache`
- `COLNAME_CACHE` is a process-wide unbounded `HashMap`
- `ThreadClonedMDB` keeps thread-local in-memory snapshots without a unified refresh contract after reload
- provider lifecycle is still tied to one-time global installation in some historical design paths

That was acceptable for the old "single process + SQLite + one computation scope" model, but it is no longer enough when the target becomes:

- supporting multiple external databases
- controlled cache invalidation
- provider reload / replace
- unified result and metadata caching
- evolving without breaking existing OML / WPL call sites

## Goals

This refactor aims to:

- establish a provider abstraction that is not SQLite-shaped
- upgrade query caching from caller-local cache to a runtime-managed cache strategy
- introduce explicit invalidation semantics for provider reload, schema changes, and backend replacement
- keep compatible facade entry points such as `query_row`, `query_named`, and `cache_query`
- provide one integration model for SQLite, PostgreSQL, and future backends

## Non-goals

This first phase does not attempt to solve:

- distributed caching
- cross-process cache consistency
- automatic SQL analyzers that decide cacheability for arbitrary queries
- full SQL dialect unification across databases
- transactional read/write consistency abstractions

The first milestone is to make the in-process architecture coherent, explicit, and extensible.

## Current pain points

### 1. Result-cache semantics are too weak

Today `cache_query` leaves key construction and lifecycle to the caller:

- the same SQL can collide across different providers
- reload can keep returning stale results if the caller still holds an old cache object
- the key only reflects `DataField` values and misses provider identity plus version

### 2. Metadata cache is unbounded

`COLNAME_CACHE` stores column names by SQL string only:

- no capacity bound
- no TTL
- no provider distinction
- no schema or generation distinction

### 3. Provider lifecycle is not replaceable enough

Historically, global provider registration was once-oriented:

- switching data sources safely is awkward
- reload lacks a formal contract
- provider state and cache state cannot be updated as one runtime unit

### 4. Thread-local snapshots are not version-aware

`ThreadClonedMDB` is effectively a snapshot cache:

- thread-local snapshots do not automatically expire after reload
- a provider switch can leave threads reading the old SQLite copy
- thread-local state is not explicitly tied to runtime generation

## Overall design

### Design principles

- providers execute queries; they do not own cache policy
- cache is shared infrastructure and should stay backend-neutral
- every cache key must carry data-source identity and version
- `generation` is the primary invalidation mechanism
- TTL is only a fallback, not the main correctness mechanism
- keep current facade APIs, but route them through a new runtime model

### Target layering

Split the internals into four layers:

1. `DatasourceRegistry`
2. `KnowledgeProvider`
3. `QueryRuntime`
4. `Facade Compatibility Layer`

Responsibilities:

- `DatasourceRegistry`
  - stores the currently active source
  - manages `source_id`
  - manages `generation`
  - handles reload / replace
- `KnowledgeProvider`
  - executes read-only queries
  - serves dictionary-table reads
  - can optionally expose schema / metadata hints
- `QueryRuntime`
  - decides cache usage
  - builds cache keys
  - owns result cache and metadata cache
  - executes unified query requests
- `Facade Compatibility Layer`
  - keeps existing `query/query_row/query_named/cache_query` entry points
  - adapts old APIs to the runtime request model

## Core abstractions

### DatasourceId

Each provider instance needs a stable identity:

```rust
pub struct DatasourceId(String);
```

Suggested values:

- SQLite authority path hash
- PostgreSQL connection-config hash
- future hashes for MySQL, HTTP, ClickHouse, and similar sources

`DatasourceId` must distinguish data sources without exposing secrets.

### Generation

Introduce a monotonic version:

```rust
pub struct Generation(u64);
```

This is the center of the invalidation contract. `generation` must advance when:

- KnowDB reload runs again
- `authority.sqlite` is rebuilt
- provider switches from SQLite to PostgreSQL or MySQL
- provider connection config changes
- whitelist changes
- an explicit schema refresh happens

Every cache key, metadata entry, and thread-local snapshot must carry `generation`.

### QueryRequest

Move to a provider-neutral internal request model instead of letting facade APIs depend on `rusqlite::ToSql`:

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

The first phase does not need every `DataField` variant to become a native DB type. It only needs the types that matter for real parameter binding.

### CachePolicy

Caching should be explicit:

```rust
pub enum CachePolicy {
    Bypass,
    UseGlobal { ttl: Option<std::time::Duration> },
    UseCallScope,
}
```

Meaning:

- `Bypass`
  - skip result cache entirely
- `UseGlobal`
  - use the shared in-process result cache
- `UseCallScope`
  - keep compatibility with the historical per-call local cache model

This lets the runtime stay conservative by default while still supporting high-ROI lookup paths.

## Provider abstraction

Add an internal trait like:

```rust
pub trait KnowledgeProvider: Send + Sync {
    fn provider_kind(&self) -> ProviderKind;
    fn datasource_id(&self) -> &DatasourceId;
    fn execute(&self, req: &QueryRequest) -> KnowledgeResult<QueryResult>;
    fn generation_hint(&self) -> Option<Generation>;
}
```

Roles:

- `provider_kind` for logs, metrics, and diagnostics
- `datasource_id` for cache isolation
- `execute` as the single execution entry point
- `generation_hint` as an optional future hook for provider-reported versioning

Dialect-specific work stays inside providers:

- SQLite binds `:name` parameters itself
- PostgreSQL rewrites `:name` into `$1/$2/...`
- MySQL keeps its own parameter and metadata handling

The runtime should not care about these dialect differences.

## Cache design

### Terminology and boundaries

The design must distinguish three different caches:

| Name | Canonical term | Current implementation | Scope | Cached content | Capacity unit | Default capacity | Controlled by |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Result cache | `result cache` | `KnowledgeRuntime.result_cache` | process-wide, provider-scoped | query result `QueryResponse` | entries | `1024 entries` | `CachePolicy::UseGlobal` |
| Call-scope cache | `local cache` | `FieldQueryCache` / `QueryLocalCache` | single evaluation / call chain | query result `RowData` | entries | `100 entries` | whether the caller uses `cache_query` |
| Metadata cache | `metadata cache` | `COLNAME_CACHE` today | process-wide, provider-scoped | column names / metadata | entries | `512 entries` | used automatically inside query paths |

Important distinctions:

- result cache stores business query results
- local cache only deduplicates repeated lookups inside one computation
- metadata cache stores auxiliary data such as column names, not result rows

### Responsibilities of the three cache layers

#### 1. result cache

This is the primary cache for reuse across requests.

Responsibilities:

- cache hotspot query results
- avoid repeated external database or SQLite work
- enable reuse across requests and across rule executions

Boundaries:

- caches query results
- does not replace per-call deduplication
- does not store metadata

Current contract:

- scope: process-wide
- key: `datasource_id + generation + query_hash + params_hash + mode`
- capacity: `1024 entries`
- enabled only via `CachePolicy::UseGlobal`
- default for normal queries remains bypass

#### 2. local cache

This is the compatibility cache for one computation scope.

Responsibilities:

- eliminate repeated lookups during one rule execution or one transform
- reduce repeated query work within a single batch / call chain
- preserve the value of historical `cache_query(...)`

Boundaries:

- does not provide cross-request consistency
- is not the primary cache
- is only visible to the call chain that holds it

Current contract:

- scope: caller-local
- key: caller-side parameter combination
- capacity: default `100 entries`, configurable by `with_capacity(size)`
- entered explicitly via `cache_query/cache_query_fields`
- on local miss, the runtime may still fall through to global result cache

#### 3. metadata cache

This is an auxiliary cache, not a business-data cache.

Responsibilities:

- cache column names and other query metadata
- avoid repeated prepare/describe overhead
- stabilize metadata reuse for result mapping

Boundaries:

- it does not cache result rows
- it cannot replace result cache
- a metadata hit does not mean a business-query hit

Current contract:

- scope: process-wide
- key: `datasource_id + generation + query_hash`
- capacity: `512 entries`
- used automatically inside SQLite / PostgreSQL / MySQL query paths
- current value is mostly column-name metadata

### Enablement semantics for the three caches

There is no single global on/off switch today:

- `result cache`
  - enabled only when the code path uses `CachePolicy::UseGlobal`
- `local cache`
  - enabled only when callers use `cache_query(...)` or pass a local cache explicitly
- `metadata cache`
  - effectively always on today
  - no separate configuration item yet

So "turning cache on" is not one boolean. It means the request path enters one or more specific cache layers.

### Current configuration contract

Only the `result cache` is currently configurable, and it must stay under the existing `knowdb.toml -> EnvTomlLoad -> KnowDbConf` loading contract:

```toml
[cache]
enabled = true
capacity = 1024
ttl_ms = 30000
```

Meaning:

- `enabled`
  - master switch for `result cache`
  - when `false`, `CachePolicy::UseGlobal` degrades to `Bypass`
- `capacity`
  - measured in entries
  - maximum number of cached query results
- `ttl_ms`
  - measured in milliseconds
  - maximum lifetime of one result-cache entry

Not covered by this config:

- `local cache`
  - still fully caller-owned
- `metadata cache`
  - still internal fixed behavior today

So `[cache]` should be read as "result cache config", not "all cache layers in wp-knowledge".

### Invalidation semantics

#### Shared rule

All three layers primarily invalidate through `generation`:

- provider reload / knowdb reload / provider replace increments generation
- new requests stop hitting old keys automatically

#### Differences

`result cache`:

- old entries are not physically removed immediately
- they become logically dead because new requests use a different generation
- stale entries are removed later by LRU / capacity pressure

`local cache`:

- when `prepare_generation(...)` detects a generation change, it calls `reset()`
- that means both logical invalidation and physical clearing happen right away

`metadata cache`:

- behaves like result cache
- old keys naturally stop matching after generation changes
- later capacity pressure clears them

### Which layers invalidate when source data changes

This is the most important boundary to make explicit.

Only provider-generation changes trigger reliable automatic invalidation.

That means:

- when reload happens
- when a new provider is installed
- when authority is rebuilt

Then:

- `result cache` naturally invalidates because generation changes
- `local cache` clears itself
- `metadata cache` naturally invalidates

But if someone changes data inside the external database and `wp-knowledge` does not reload:

- `result cache` does not automatically invalidate
- `local cache` does not discover external data changes either
- `metadata cache` usually stays the same unless schema also changes and reload follows

So the current consistency boundary must be stated plainly:

- cache consistency follows `provider generation`
- it does not follow real-time external database changes

Not present today:

- TTL-driven correctness invalidation
- CDC-based invalidation
- table-version probing
- background invalidation on upstream data changes

### 1. Result cache

Use a mature concurrent cache such as `moka::sync::Cache` instead of treating `FieldQueryCache` as the main cache implementation.

#### Key structure

```rust
pub struct ResultCacheKey {
    pub datasource_id: DatasourceId,
    pub generation: Generation,
    pub query_hash: u64,
    pub params_hash: u64,
    pub mode: QueryModeTag,
}
```

Requirements:

- must include `datasource_id`
- must include `generation`
- must not cache by SQL string only
- parameters must be normalized before hashing

#### Normalization rules

- keep parameter names
- keep parameter values
- drop formatting-only metadata from `DataField`
- encode `NULL`, bool, number, and text values stably

#### Good fit

- dictionary lookup
- hotspot small queries in rule evaluation
- read-only queries over whitelisted tables

#### Default policy

- result cache should remain opt-in
- callers or rule layers enable it explicitly

#### Invalidation

- primary: `generation`
- secondary: TTL
- capacity protection: `max_capacity`

### 2. Call-scope cache

`FieldQueryCache` should not be deleted immediately. It should be demoted into a local compatibility cache.

Why keep it:

- OML and rule engines often repeat the same lookup within one transformation
- that cache still has very high ROI
- it does not need concurrency support

But its positioning must change:

- it no longer carries global consistency semantics
- it is no longer the primary cache
- it only deduplicates repeated lookups within one computation

Recommended rename:

- `QueryLocalCache`

### 3. Column-name / metadata cache

Replace `COLNAME_CACHE` with a unified metadata cache, ideally also backed by `moka`.

#### Key structure

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

Phase 1 can fill only column names and still fully cover the old `COLNAME_CACHE` behavior.

#### Policy

- capacity bound is required
- TTL is optional
- generation drives correctness, just like result cache

## ThreadClonedMDB changes

This is non-negotiable. The thread-local in-memory SQLite database is effectively a hidden large cache. If it is not brought under the same generation model, outer cache correctness is still incomplete after reload.

Recommended thread-local state:

```rust
struct ThreadLocalState {
    generation: Generation,
    conn: Connection,
}
```

Access logic:

- first access on a thread builds `conn`
- if local generation differs from runtime generation:
  - drop the old `conn`
  - rebuild from the new authority/provider

That guarantees:

- threads move to the new snapshot after authority rebuild
- provider switches do not keep reading the old SQLite copy

## Facade evolution

### Compatibility goal

Keep these APIs in phase 1:

- `query`
- `query_row`
- `query_named`
- `query_named_fields`
- `cache_query`

Internally:

- always build a unified `QueryRequest`
- hand it to `QueryRuntime`
- let the runtime decide whether global cache is consulted

### Redefining `cache_query`

Treat `cache_query` explicitly as a compatibility API:

- it keeps supporting historical local cache behavior
- it can optionally stack on top of global cache
- its long-term role should become smaller, not larger

Recommended execution order:

1. check local cache
2. if miss and global cache is allowed, check global result cache
3. if still miss, execute the provider query
4. write back to global cache
5. write back to local cache

This keeps per-call deduplication while also enabling cross-request reuse.

## Reload and runtime management

Introduce a runtime state object:

```rust
pub struct KnowledgeRuntime {
    pub registry: ArcSwap<ProviderHandle>,
    pub result_cache: moka::sync::Cache<ResultCacheKey, Arc<QueryResult>>,
    pub metadata_cache: moka::sync::Cache<MetadataCacheKey, Arc<QueryMetadata>>,
}
```

`ProviderHandle` contains:

- provider instance
- datasource_id
- generation
- whitelist

Reload flow:

1. build the new provider
2. generate a new `datasource_id` or reuse the old one when appropriate
3. `generation += 1`
4. atomically swap the registry handle
5. do not synchronously purge every cache entry; rely on generation-based invalidation

Optional asynchronous cleanup can still call:

- `invalidate_entries_if(...)`
- `run_pending_tasks()`

But correctness must not depend on that cleanup.

## Observability

Without metrics, this design will be hard to operate and evaluate.

Suggested metrics and logs:

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

At minimum, debug logs should be able to answer:

- which provider served the current query
- which generation is active
- whether the hit came from local cache or global cache
- whether a thread-local snapshot had to be rebuilt

## Performance validation

To keep the discussion grounded, the repository already includes three repeatable cache performance paths:

- in-memory provider comparison: `tests/test-cache-perf.sh`
- real PostgreSQL provider comparison: `tests/test-postgres-provider.sh`
- real MySQL provider comparison: `tests/test-mysql-provider.sh`

Each comparison uses the same three paths:

- `bypass`
  - skip result cache completely
- `global_cache`
  - use runtime-level result cache
- `local_cache`
  - check call-scope local cache first, then fall through to runtime result cache

### Default sample parameters

- `WP_KDB_PERF_ROWS=10000`
- `WP_KDB_PERF_OPS=10000`
- `WP_KDB_PERF_HOTSET=128`

Meaning:

- prepare `10000` rows
- run `10000` lookups
- actual hotspot key set is only `128`, so cache hit rate is intentionally high for lookup-heavy workloads

### Measured results

The following numbers come from actual runs in the current repository on the current development machine with local Docker-backed providers.

#### 1. In-memory provider

Entry point:

```bash
./tests/test-cache-perf.sh
```

One sample run:

| Scenario | Elapsed | QPS | Result Cache | Local Cache |
| --- | ---: | ---: | --- | --- |
| `bypass` | `1204 ms` | `99,658` | `hit=0 miss=0` | `hit=0 miss=0` |
| `global_cache` | `116 ms` | `1,034,157` | `hit=119872 miss=128` | `hit=0 miss=0` |
| `local_cache` | `71 ms` | `1,686,998` | `hit=0 miss=128` | `hit=119872 miss=128` |

Speedup:

- `global_cache vs bypass`: `10.38x`
- `local_cache vs bypass`: `16.93x`

Interpretation:

- the in-memory provider is already fast, so cache mainly removes repeated SQL execution plus repeated metadata / row assembly
- `local_cache` being slightly faster than `global_cache` matches the expectation for repeated lookups inside one computation

#### 2. PostgreSQL provider

Entry point:

```bash
./tests/test-postgres-provider.sh
```

One sample run:

| Scenario | Elapsed | QPS | Result Cache | Local Cache |
| --- | ---: | ---: | --- | --- |
| `bypass` | `5201 ms` | `1,923` | `hit=0 miss=0` | `hit=0 miss=0` |
| `global_cache` | `79 ms` | `125,212` | `hit=9872 miss=128` | `hit=0 miss=0` |
| `local_cache` | `73 ms` | `136,901` | `hit=0 miss=128` | `hit=9872 miss=128` |

Speedup:

- `global_cache vs bypass`: `65.13x`
- `local_cache vs bypass`: `71.21x`

Interpretation:

- cache gains are much larger on an external database than on the in-memory provider
- the win is not from complex SQL alone, but from skipping binding, connection checkout, network round-trip, and result deserialization
- for hotspot small queries, runtime result cache already flattens most external-database cost

#### 3. MySQL provider

Entry point:

```bash
./tests/test-mysql-provider.sh
```

One sample run:

| Scenario | Elapsed | QPS | Result Cache | Local Cache |
| --- | ---: | ---: | --- | --- |
| `bypass` | `5757 ms` | `1,737` | `hit=0 miss=0` | `hit=0 miss=0` |
| `global_cache` | `82 ms` | `121,016` | `hit=9872 miss=128` | `hit=0 miss=0` |
| `local_cache` | `73 ms` | `136,587` | `hit=0 miss=128` | `hit=9872 miss=128` |

Speedup:

- `global_cache vs bypass`: `69.67x`
- `local_cache vs bypass`: `78.63x`

Interpretation:

- MySQL reaches the same practical conclusion as PostgreSQL: cache gives order-of-magnitude gains for hotspot lookups
- `local_cache` remains slightly faster than `global_cache`, which confirms the value of caller-scoped deduplication for repeated keys inside one rule execution

### Reading the results

These three result groups support a few clear conclusions:

- cache is not "maybe useful"; it is decisive for hotspot lookup workloads
- for external providers, connection pooling alone is not enough because repeated query work still dominates
- `global_cache` already covers cross-request reuse value
- `local_cache` is still worth keeping, especially for repeated keys inside one transformation
- because generation is already part of runtime cache semantics, these gains are not bought by sacrificing correctness

### Caveats

- these measurements use a hotspot-heavy workload and do not mean every SQL query should be cached by default
- cache value drops for non-deterministic queries, low-reuse queries, or large result sets
- the default parameters are tuned for daily regression checks; closer production simulation needs real hotspot distributions
- the numbers here describe measured current behavior, not long-term performance commitments

### Recommended engineering conclusions

Based on the current measurements:

- keep result cache disabled by default and enable it only for known hotspot queries
- keep `cache_query` in the compatibility layer as the main local-cache entry point for OML / rule execution
- for PostgreSQL and MySQL, migrate high-frequency lookup SQL explicitly to `UseGlobal` or `cache_query`
- focus future optimization work on converging and configuring cache strategy before micro-tuning driver-level SQL overhead

## Migration steps

Break the work into five phases instead of rewriting everything at once.

### Phase 1: introduce runtime and generation

- add `KnowledgeRuntime`
- upgrade provider registration from `OnceLock` to a replaceable runtime handle
- make the provider handle explicitly carry `datasource_id/generation`
- route facade dispatch through the runtime

### Phase 2: introduce unified `QueryRequest`

- add `QueryRequest/QueryParam/QueryResult`
- migrate SQLite / PostgreSQL providers to a unified `execute`
- adapt `query_named` from the `ToSql` compatibility layer into `QueryParam`

### Phase 3: introduce global result cache and metadata cache

- adopt `moka`
- replace `COLNAME_CACHE` with metadata cache first
- enable result cache first only for explicitly cacheable queries

### Phase 4: update `ThreadClonedMDB`

- store generation on thread-local connections
- rebuild automatically when generation changes
- attach thread-local snapshots to the runtime provider handle model

### Phase 5: shrink the historical compatibility layer

- reduce the role of `cache_query`
- rename `FieldQueryCache` to `QueryLocalCache`
- migrate more hotspot lookups explicitly onto global cache policy

## Risks and trade-offs

### Risk 1: cache contamination

If `datasource_id` or `generation` is omitted, results from different providers can leak into each other.

Conclusion:

- both fields must exist in every global cache key

### Risk 2: stale reads after reload

If the provider is replaced without rebuilding thread-local snapshots, SQLite thread snapshots can stay stale.

Conclusion:

- `ThreadClonedMDB` must participate in generation semantics

### Risk 3: over-caching by default

Not every read-only SQL query against an external database is deterministic enough to cache safely.

Conclusion:

- keep result cache opt-in
- make cache behavior explicit via policy

### Risk 4: changing public APIs too aggressively

Deleting facade APIs immediately would force large upstream changes.

Conclusion:

- keep the facade
- refactor the runtime first

## Suggested dependencies

```toml
moka = { version = "0.12", features = ["sync"] }
arc-swap = "1"
```

Notes:

- `moka` for result cache and metadata cache
- `arc-swap` for low-cost atomic switching of the active provider handle

## Final judgment

If `wp-knowledge` wants to support SQLite and external databases sustainably, the architectural center must move from "SQLite-oriented query helpers" to "a data-source runtime with explicit version semantics".

The key is not just swapping one cache library. The refactor must land these four things together:

- provider abstraction that is no longer SQLite-shaped
- runtime-owned cache strategy
- generation-owned invalidation semantics
- thread-local snapshots under unified version management

Only when these four pieces hold together can `wp-knowledge` support external databases with a maintainable and correct cache model.
