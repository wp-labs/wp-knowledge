# Async Provider 性能测试与结论

中文 | [English](../../en/performance/async-provider.md)

`wp-knowledge` 切换到 tokio 原生 async DB driver 后，这份文档记录两类性能结论：

- `wp-knowledge` provider 层：比较 PostgreSQL / MySQL 在 `sync` 与 `async` 两条调用路径下的 `QPS / P50 / P95`
- `wp-oml` 层：比较接入 `wp-knowledge` 之后，async OML 在 PostgreSQL / MySQL provider 下的整体吞吐变化

核心结论：

- provider 层的 `sync` / `async` 性能基本持平，async 略优，但不是数量级变化
- `wp-oml` 一旦进入真实 SQL/effect 路径，吞吐会下降到纯 OML 的约 `8% ~ 9%`
- 在并发压测下，`cache` 会把 provider 吞吐从 `2.7K ~ 2.9K QPS` 拉升到 `60万 ~ 200万+ QPS`
- `async OML + knowledge + cache` 在 `16 ~ 64` 并发下可稳定达到 `170万 ~ 280万 QPS`
- 主要成本在整条 `OML -> SQL -> provider -> row decode -> 回填 OML` 链路，不在单个 PG/MySQL driver 切换本身

## 测试口径

- 数据规模：`rows=10000`
- 请求规模：`ops=10000`
- 热点集合：`hotset=128`
- PostgreSQL 连接串：`postgres://postgres:demo@127.0.0.1:5432/postgres`
- MySQL 连接串：`mysql://root:demo@127.0.0.1:3306/demo`

Provider 层对比使用同一条查询：

```sql
SELECT value FROM <perf_table> WHERE id=:id
```

OML 层对比使用两类模型：

- 纯 OML：

```oml
name : bench
---
V = read(id) ;
```

- 接入知识库 SQL：

```oml
name : bench
---
V = select value from <perf_table> where id = read(id) ;
```

说明：

- `sync` 路径指同步 facade / 同步 provider 调用路径
- `async` 路径指 async facade / async provider 调用路径
- 本文“并发”部分使用 `concurrency = 1 / 4 / 16 / 64`
- 并发 `cache` 测试口径使用 `ops=20000`
- 本文数值来自同一轮本地实测，适合做相对比较，不应直接作为跨环境 SLA

## 结果

### wp-knowledge

#### PostgreSQL

| mode | QPS | P50 | P95 |
| --- | ---: | ---: | ---: |
| sync | 2690 | 362.25 us | 457.79 us |
| async | 2701 | 358.88 us | 461.29 us |

结论：

- `sync` 与 `async` 两条调用路径几乎持平
- `async` 在 `QPS / P50` 上略优，但不是数量级变化
- 切到原生 async driver 之后，主要收益是运行模型更合理，而不是单条查询 latency 的显著下降

#### MySQL

| mode | QPS | P50 | P95 |
| --- | ---: | ---: | ---: |
| sync | 2616 | 370.50 us | 478.42 us |
| async | 2677 | 362.46 us | 461.54 us |

结论：

- MySQL 与 PostgreSQL 的趋势一致
- `async` 略快于 `sync`，但整体仍属于“接近持平”

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

结论：

- 一旦 OML 节点真的进入外部 SQL/effect 路径，吞吐会显著低于纯 OML
- PostgreSQL / MySQL 的结果非常接近，说明当前主要成本在整条 `OML -> SQL -> provider -> row decode -> 回填 OML` 链路，而不是某一个具体 provider 的单点实现
- 这个差值是“纯内存表达式求值”与“真实外部查询求值”的差值，不应简单归因于 async driver

## 与 Cache Perf 的关系

同一轮测试里，`wp-knowledge` provider cache perf 结果如下：

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

解释：

- 裸查 provider 的吞吐大约在 `2.6K ~ 2.8K QPS`
- 命中 cache 后可以提升到 `13万 ~ 18万 QPS`
- `wp-oml` 接 provider 的 `10万+ QPS` 已经接近“带缓存查询”的真实上限，不属于异常回退

## 并发下的 Cache 表现

这一部分回答的是“`async OML` 接知识库后，再叠加 `cache`，在并发下是什么表现”。

### wp-knowledge Provider 层

#### PostgreSQL

| mode | concurrency=1 | concurrency=4 | concurrency=16 | concurrency=64 |
| --- | ---: | ---: | ---: | ---: |
| async_bypass | 2,689 QPS | 7,757 QPS | 15,156 QPS | 29,143 QPS |
| async_global_cache | 245,894 QPS | 620,241 QPS | 1,010,171 QPS | 1,272,680 QPS |
| async_local_cache | 265,450 QPS | 770,683 QPS | 1,434,287 QPS | 2,092,479 QPS |

延迟观察：

- `async_bypass` 在 `concurrency=64` 时，`P50/P95` 分别升到 `2120 / 2873 us`
- `async_global_cache` 在 `concurrency=64` 时，`P50/P95` 仍只有 `5.00 / 16.96 us`
- `async_local_cache` 在 `concurrency=64` 时，`P50/P95` 维持在 `0.96 / 1.58 us`

#### MySQL

| mode | concurrency=1 | concurrency=4 | concurrency=16 | concurrency=64 |
| --- | ---: | ---: | ---: | ---: |
| async_bypass | 2,691 QPS | 7,465 QPS | 9,504 QPS | 9,432 QPS |
| async_global_cache | 273,408 QPS | 653,481 QPS | 679,974 QPS | 1,033,193 QPS |
| async_local_cache | 298,175 QPS | 810,693 QPS | 1,167,878 QPS | 1,289,660 QPS |

延迟观察：

- `async_bypass` 在 `concurrency=64` 时，`P50/P95` 升到 `6749 / 7174 us`
- `async_global_cache` 在 `concurrency=64` 时，`P50/P95` 仍只有 `3.29 / 9.58 us`
- `async_local_cache` 在 `concurrency=64` 时，`P50/P95` 维持在 `0.96 / 1.21 us`

结论：

- provider 裸查在并发下仍然受外部数据库往返限制，吞吐提升有限
- 一旦命中 `global cache` 或 `local cache`，provider 层吞吐会直接进入 `百万 QPS` 区间
- `local cache` 仍然是最低延迟路径；`global cache` 次之；裸查最慢

### wp-oml 层

#### PostgreSQL Provider + Cache

| scenario | concurrency=1 | concurrency=4 | concurrency=16 | concurrency=64 |
| --- | ---: | ---: | ---: | ---: |
| pure_async | 933,130 QPS | 1,561,580 QPS | 4,769,902 QPS | 8,428,743 QPS |
| async_provider_cache | 156,354 QPS | 518,125 QPS | 1,672,690 QPS | 2,788,461 QPS |

延迟观察：

- `async_provider_cache` 在 `concurrency=64` 时，`P50/P95 = 4.17 / 10.04 us`
- `provider_vs_pure` 从单线程的 `0.17x` 提升到并发下约 `0.33x ~ 0.35x`

#### MySQL Provider + Cache

| scenario | concurrency=1 | concurrency=4 | concurrency=16 | concurrency=64 |
| --- | ---: | ---: | ---: | ---: |
| pure_async | 762,170 QPS | 1,532,993 QPS | 4,906,971 QPS | 7,656,601 QPS |
| async_provider_cache | 171,774 QPS | 575,005 QPS | 1,738,545 QPS | 2,747,599 QPS |

延迟观察：

- `async_provider_cache` 在 `concurrency=64` 时，`P50/P95 = 4.29 / 10.29 us`
- `provider_vs_pure` 在并发下稳定在 `0.35x ~ 0.38x`

结论：

- `async OML + knowledge + cache` 在并发下已经明显脱离“数据库裸查”区间，进入 `百万 QPS` 量级
- PostgreSQL / MySQL 的并发表现非常接近，说明主要瓶颈仍是 `OML -> SQL -> provider -> 结果映射 -> 回填` 的固定成本
- 在 `16 ~ 64` 并发下，OML 端到端吞吐大致落在 `170万 ~ 280万 QPS`
- 这也说明当前 `cache` 已经有效削平了数据库 I/O，剩余优化空间主要在 OML 自身调用链

## 总结

1. `wp-knowledge` 切换到 tokio 原生 async driver 后，provider 层 `sync` 和 `async` 查询性能基本持平，`async` 略优。
2. 这次改造的主要价值是 async 运行模型正确化、调度语义合理化，而不是把单条数据库查询 latency 大幅压低。
3. `wp-oml` 一旦进入真实知识库查询路径，吞吐会降到纯 OML 的大约 `8% ~ 9%`，这是 effect 路径本身的成本，不是单纯由 async driver 引起。
4. 后续如果继续优化，重点应放在：
   - OML 到 provider 的调用链固定成本
   - `wp-knowledge` 命中 cache 时的固定开销
   - SQL 节点的参数构造、结果映射和对象回填成本

## 复现命令

PostgreSQL provider 层：

```bash
KEEP_DB=1 RUSTC_WRAPPER= bash tests/test-postgres-provider.sh
```

MySQL provider 层：

```bash
KEEP_DB=1 RUSTC_WRAPPER= bash tests/test-mysql-provider.sh
```

PostgreSQL OML 层：

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

MySQL OML 层：

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

PostgreSQL 并发 cache perf：

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

MySQL 并发 cache perf：

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

PostgreSQL OML 并发 cache perf：

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

MySQL OML 并发 cache perf：

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
