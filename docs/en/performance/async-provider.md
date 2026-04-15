# Async Provider Performance

[中文](../../zh/performance/async-provider.md) | English

After `wp-knowledge` switched to tokio-native async database drivers, this document records two groups of performance conclusions:

- `wp-knowledge` provider layer: compare PostgreSQL / MySQL under `sync` and `async` paths with `QPS / P50 / P95`
- `wp-oml` layer: compare end-to-end throughput changes after `wp-knowledge` is used from async OML under PostgreSQL / MySQL providers

Core conclusions:

- provider-layer `sync` and `async` performance is nearly identical; async is slightly better, but not by an order of magnitude
- once `wp-oml` enters a real SQL/effect path, throughput drops to roughly `8% ~ 9%` of pure OML
- under concurrent load, `cache` raises provider throughput from about `2.7K ~ 2.9K QPS` to `600K ~ 2M+ QPS`
- `async OML + knowledge + cache` can sustain about `1.7M ~ 2.8M QPS` under `16 ~ 64` concurrency
- the main cost lives in the full `OML -> SQL -> provider -> row decode -> OML write-back` pipeline, not in switching PG/MySQL drivers

## Test setup

- Dataset size: `rows=10000`
- Request count: `ops=10000`
- Hotset size: `hotset=128`
- PostgreSQL URL: `postgres://postgres:demo@127.0.0.1:5432/postgres`
- MySQL URL: `mysql://root:demo@127.0.0.1:3306/demo`

The provider-layer comparison uses the same query:

```sql
SELECT value FROM <perf_table> WHERE id=:id
```

The OML-layer comparison uses two model shapes:

- Pure OML:

```oml
name : bench
---
V = read(id) ;
```

- OML with knowledge SQL:

```oml
name : bench
---
V = select value from <perf_table> where id = read(id) ;
```

Notes:

- `sync` means the synchronous facade / provider path.
- `async` means the async facade / async provider path.
- The concurrency section uses `concurrency = 1 / 4 / 16 / 64`.
- Concurrent cache tests use `ops=20000`.
- The numbers come from one local measurement round and should be treated as relative comparisons, not portable SLA commitments.

## Results

### wp-knowledge

#### PostgreSQL

| mode | QPS | P50 | P95 |
| --- | ---: | ---: | ---: |
| sync | 2690 | 362.25 us | 457.79 us |
| async | 2701 | 358.88 us | 461.29 us |

Conclusion:

- the `sync` and `async` paths are effectively tied
- `async` is slightly better on `QPS / P50`, but not by an order of magnitude
- the main win of the native async driver is a cleaner runtime model, not a dramatic single-query latency reduction

#### MySQL

| mode | QPS | P50 | P95 |
| --- | ---: | ---: | ---: |
| sync | 2616 | 370.50 us | 478.42 us |
| async | 2677 | 362.46 us | 461.54 us |

Conclusion:

- MySQL follows the same trend as PostgreSQL
- `async` is a little faster than `sync`, but still within the same rough range

### wp-oml

#### PostgreSQL Provider

| scenario | QPS |
| --- | ---: |
| pure async OML | 1,233,870 |
| async OML + PostgreSQL provider | 100,254 |

`provider_vs_pure = 0.08x`

#### MySQL Provider

| scenario | QPS |
| --- | ---: |
| pure async OML | 1,185,021 |
| async OML + MySQL provider | 107,818 |

`provider_vs_pure = 0.09x`

Conclusion:

- once an OML node enters a real external SQL/effect path, throughput is much lower than pure OML
- PostgreSQL and MySQL are very close here, which suggests the fixed cost is the full `OML -> SQL -> provider -> row decode -> OML write-back` pipeline rather than one specific provider implementation
- the gap should be understood as the difference between pure in-memory evaluation and real external queries, not as an async-driver-only effect

## Relationship to cache perf

In the same round of measurements, provider cache performance looked like this:

### PostgreSQL

| scenario | QPS |
| --- | ---: |
| bypass | 2,784 |
| global_cache | 131,508 |
| local_cache | 151,501 |

### MySQL

| scenario | QPS |
| --- | ---: |
| bypass | 2,674 |
| global_cache | 162,610 |
| local_cache | 177,758 |

Interpretation:

- raw provider throughput is about `2.6K ~ 2.8K QPS`
- hitting cache raises that to about `130K ~ 180K QPS`
- the `100K+ QPS` seen from `wp-oml` with a provider is already close to the realistic cached-query upper bound, so it is not an unexpected regression

## Cache behavior under concurrency

This section answers a different question: after `async OML` is connected to knowledge queries, what happens when cache is added under concurrent load?

### wp-knowledge provider layer

#### PostgreSQL

| mode | concurrency=1 | concurrency=4 | concurrency=16 | concurrency=64 |
| --- | ---: | ---: | ---: | ---: |
| async_bypass | 2,689 QPS | 7,757 QPS | 15,156 QPS | 29,143 QPS |
| async_global_cache | 245,894 QPS | 620,241 QPS | 1,010,171 QPS | 1,272,680 QPS |
| async_local_cache | 265,450 QPS | 770,683 QPS | 1,434,287 QPS | 2,092,479 QPS |

Latency notes:

- `async_bypass` rises to `P50/P95 = 2120 / 2873 us` at `concurrency=64`
- `async_global_cache` stays at only `5.00 / 16.96 us`
- `async_local_cache` stays at `0.96 / 1.58 us`

#### MySQL

| mode | concurrency=1 | concurrency=4 | concurrency=16 | concurrency=64 |
| --- | ---: | ---: | ---: | ---: |
| async_bypass | 2,691 QPS | 7,465 QPS | 9,504 QPS | 9,432 QPS |
| async_global_cache | 273,408 QPS | 653,481 QPS | 679,974 QPS | 1,033,193 QPS |
| async_local_cache | 298,175 QPS | 810,693 QPS | 1,167,878 QPS | 1,289,660 QPS |

Latency notes:

- `async_bypass` rises to `P50/P95 = 6749 / 7174 us` at `concurrency=64`
- `async_global_cache` stays at `3.29 / 9.58 us`
- `async_local_cache` stays at `0.96 / 1.21 us`

Conclusion:

- uncached provider queries remain bounded by external database round trips
- once `global cache` or `local cache` is hit, provider throughput moves into the million-QPS range
- `local cache` is still the lowest-latency path; `global cache` is second; bypass is slowest

### wp-oml layer

#### PostgreSQL Provider + Cache

| scenario | concurrency=1 | concurrency=4 | concurrency=16 | concurrency=64 |
| --- | ---: | ---: | ---: | ---: |
| pure_async | 933,130 QPS | 1,561,580 QPS | 4,769,902 QPS | 8,428,743 QPS |
| async_provider_cache | 156,354 QPS | 518,125 QPS | 1,672,690 QPS | 2,788,461 QPS |

Latency notes:

- `async_provider_cache` reaches `P50/P95 = 4.17 / 10.04 us` at `concurrency=64`
- `provider_vs_pure` improves from `0.17x` in single-thread mode to roughly `0.33x ~ 0.35x` under concurrency

#### MySQL Provider + Cache

| scenario | concurrency=1 | concurrency=4 | concurrency=16 | concurrency=64 |
| --- | ---: | ---: | ---: | ---: |
| pure_async | 762,170 QPS | 1,532,993 QPS | 4,906,971 QPS | 7,656,601 QPS |
| async_provider_cache | 171,774 QPS | 575,005 QPS | 1,738,545 QPS | 2,747,599 QPS |

Latency notes:

- `async_provider_cache` reaches `P50/P95 = 4.29 / 10.29 us` at `concurrency=64`
- `provider_vs_pure` stays around `0.35x ~ 0.38x` under concurrency

Conclusion:

- `async OML + knowledge + cache` is clearly out of the raw-database zone and into the million-QPS zone under concurrency
- PostgreSQL and MySQL remain very close, which again points to fixed OML-to-provider overhead rather than a database-specific bottleneck
- end-to-end OML throughput lands around `1.7M ~ 2.8M QPS` under `16 ~ 64` concurrency
- cache has already flattened most database I/O cost; the remaining work is mostly in OML's own call path

## Summary

1. After switching `wp-knowledge` to tokio-native async drivers, provider-layer `sync` and `async` query performance stays nearly identical, with async slightly ahead.
2. The main value of the change is a correct async runtime model and more reasonable scheduling semantics, not a dramatic latency drop for one query.
3. Once `wp-oml` enters a real knowledge-query path, throughput drops to around `8% ~ 9%` of pure OML. That is the cost of the effect path itself, not just the async driver.
4. Future optimization should focus on:
   - fixed overhead in the OML-to-provider call chain
   - fixed overhead when `wp-knowledge` hits cache
   - parameter construction, result mapping, and object write-back for SQL nodes

## Reproduction commands

PostgreSQL provider layer:

```bash
KEEP_DB=1 RUSTC_WRAPPER= bash tests/test-postgres-provider.sh
```

MySQL provider layer:

```bash
KEEP_DB=1 RUSTC_WRAPPER= bash tests/test-mysql-provider.sh
```

PostgreSQL OML layer:

```bash
WP_KDB_TEST_POSTGRES_URL='postgres://postgres:demo@127.0.0.1:5432/postgres' \
WP_KDB_PERF_ROWS=10000 \
WP_KDB_PERF_OPS=10000 \
WP_KDB_PERF_HOTSET=128 \
RUSTC_WRAPPER= \
cargo test --manifest-path ../wp-motor/Cargo.toml -p wp-oml \
  --test knowledge_async_perf oml_async_postgres_provider_throughput \
  -- --ignored --nocapture
```

MySQL OML layer:

```bash
WP_KDB_TEST_MYSQL_URL='mysql://root:demo@127.0.0.1:3306/demo' \
WP_KDB_PERF_ROWS=10000 \
WP_KDB_PERF_OPS=10000 \
WP_KDB_PERF_HOTSET=128 \
RUSTC_WRAPPER= \
cargo test --manifest-path ../wp-motor/Cargo.toml -p wp-oml \
  --test knowledge_async_perf oml_async_mysql_provider_throughput \
  -- --ignored --nocapture
```

PostgreSQL concurrent cache perf:

```bash
WP_KDB_TEST_POSTGRES_URL='postgres://postgres:demo@127.0.0.1:5432/postgres' \
WP_KDB_PERF_ROWS=10000 \
WP_KDB_PERF_OPS=20000 \
WP_KDB_PERF_HOTSET=128 \
WP_KDB_PERF_CONCURRENCY='1,4,16,64' \
RUSTC_WRAPPER= \
cargo test --manifest-path Cargo.toml \
  --test postgres_provider postgres_provider_async_cache_concurrency_perf \
  -- --ignored --nocapture
```

MySQL concurrent cache perf:

```bash
WP_KDB_TEST_MYSQL_URL='mysql://root:demo@127.0.0.1:3306/demo' \
WP_KDB_PERF_ROWS=10000 \
WP_KDB_PERF_OPS=20000 \
WP_KDB_PERF_HOTSET=128 \
WP_KDB_PERF_CONCURRENCY='1,4,16,64' \
RUSTC_WRAPPER= \
cargo test --manifest-path Cargo.toml \
  --test mysql_provider mysql_provider_async_cache_concurrency_perf \
  -- --ignored --nocapture
```

PostgreSQL OML concurrent cache perf:

```bash
WP_KDB_TEST_POSTGRES_URL='postgres://postgres:demo@127.0.0.1:5432/postgres' \
WP_KDB_PERF_ROWS=10000 \
WP_KDB_PERF_OPS=20000 \
WP_KDB_PERF_HOTSET=128 \
WP_KDB_PERF_CONCURRENCY='1,4,16,64' \
RUSTC_WRAPPER= \
cargo test --manifest-path ../wp-motor/Cargo.toml -p wp-oml \
  --test knowledge_async_perf oml_async_postgres_provider_cache_concurrency \
  -- --ignored --nocapture
```

MySQL OML concurrent cache perf:

```bash
WP_KDB_TEST_MYSQL_URL='mysql://root:demo@127.0.0.1:3306/demo' \
WP_KDB_PERF_ROWS=10000 \
WP_KDB_PERF_OPS=20000 \
WP_KDB_PERF_HOTSET=128 \
WP_KDB_PERF_CONCURRENCY='1,4,16,64' \
RUSTC_WRAPPER= \
cargo test --manifest-path ../wp-motor/Cargo.toml -p wp-oml \
  --test knowledge_async_perf oml_async_mysql_provider_cache_concurrency \
  -- --ignored --nocapture
```
