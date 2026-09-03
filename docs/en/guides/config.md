# KnowDB Configuration

[中文](../../zh/guides/config.md) | English

Remember only three things:

1. only `version = 2` is supported
2. choose one mode only: directory-based SQLite authority, or external PostgreSQL / MySQL
3. `[cache]` controls `result cache` only

## Two modes

- Static CSV assets: use directory-based SQLite authority
- Data already in PostgreSQL / MySQL: use external provider

## Minimal examples

### Directory-based SQLite authority

```toml
version = 2
base_dir = "."

[default]
transaction = true
batch_size = 2000
on_error = "fail"

[csv]
has_header = true
delimiter = ","
encoding = "utf-8"
trim = true

[cache]
enabled = true
capacity = 1024
ttl_ms = 30000

[[tables]]
name = "example"
dir = "example"
enabled = true
columns.by_header = ["name", "pinying"]

[tables.expected_rows]
min = 5
max = 100
```

### External PostgreSQL / MySQL

Single database:

```toml
version = 2

[cache]
enabled = true
capacity = 1024
ttl_ms = 30000

[provider.sqldb]
kind = "postgres"
connection_uri = "postgres://user:${DB_PASSWORD}@127.0.0.1:5432/demo"
pool_size = 8
```

Switch `kind = "postgres"` to `kind = "mysql"` and use a MySQL connection URI if needed.

**Multiple databases**: use the array form `[[provider.sqldb]]`, one `name` per database:

```toml
version = 2

[[provider.sqldb]]
name = "geo"
kind = "postgres"
connection_uri = "postgres://user:${GEO_DB_PASSWORD}@127.0.0.1:5432/geo_db"
pool_size = 8

[[provider.sqldb]]
name = "asset"
kind = "postgres"
connection_uri = "postgres://user:${ASSET_DB_PASSWORD}@127.0.0.1:5432/asset_db"
pool_size = 8
```

- Without a `name` (or with a single `[provider.sqldb]`), the effective name is `default`, used as the default database.
- OML queries without a prefix go to the default database; prefixed queries target a specific one:

```oml
country = select country_name from geo.public.ip_geo_city where ip_num = @ip_num;
-- routes to the provider named "geo"; the geo. prefix is stripped before hitting PostgreSQL.
```

- `name` allows only `[A-Za-z0-9_]`; duplicates raise a configuration error.

**PostgreSQL per-connection session initialization (optional)**: issue fixed `SET` commands on every connection from the pool to stabilize execution plans (e.g. lock IP-geolocation lookups to generic plans so parameterized queries stop re-planning). Only `kind = "postgres"` supports this:

```toml
[provider.sqldb.postgres_session]
plan_cache_mode = "force_generic_plan"   # auto / force_generic_plan / force_custom_plan
jit = false                               # disable JIT to avoid compile overhead on small queries
application_name = "ip_geo_service"       # visible in pg_stat_activity (≤ 63 bytes)
```

- All three are optional; omitted items are not sent and keep the database default.
- When applied: after the pool is created, each new connection (including recycles and reconnects) runs the `SET`s in `after_connect`; startup then reads `current_setting` through the same pool and compares against expectations, failing with the offending parameter on mismatch.
- `plan_cache_mode` requires PostgreSQL 12+; `jit` / `application_name` work on earlier versions.
- Unknown fields in this block are rejected (`deny_unknown_fields`); there is no arbitrary-SQL entry point.

## Which settings take effect

| Mode | Active settings | Not part of the main flow |
| --- | --- | --- |
| Directory-based SQLite authority | `version` `base_dir` `[default]` `[csv]` `[cache]` `[[tables]]` | `authority_uri` is not read from `knowdb.toml` |
| External PostgreSQL / MySQL | `version` `[provider.sqldb]` (or `[[provider.sqldb]]`) `[cache]` | `base_dir` `[default]` `[csv]` `[[tables]]` |
| Intranet network knowledge `[intranet_nets]` | effective in both modes (injected on knowdb.toml parse) | — |

## Key fields

### Top level

- `version`
  - required, must be `2`
- `base_dir`
  - root for table directories
- `[provider.sqldb]` / `[[provider.sqldb]]`
  - only for external provider mode
- `[[tables]]`
  - only for directory-based SQLite authority mode

### `[default]`

- `transaction`
  - default `true`
- `batch_size`
  - default `2000`
- `on_error`
  - `fail` stops
  - `skip` ignores bad rows

### `[csv]`

- `has_header`
  - default `true`
- `delimiter`
  - keep it to one character
- `encoding`
  - only `utf-8` is supported
- `trim`
  - default `true`

### `[cache]`

- `enabled`
  - default `true`
- `capacity`
  - default `1024`, in entries
- `ttl_ms`
  - default `30000`

### `[intranet_nets]`

Intranet network knowledge config (feeds `intranet_ip` / `access_direct` intranet-side checks).

- `enabled`
  - default `true` (config takes effect when present)
  - set `false` to ignore this section and use the built-in default networks
- `mode`
  - `add`: append external networks to the built-in defaults (default, recommended)
  - `replace`: replace the built-in defaults entirely with the external networks
- `nets`
  - list of intranet networks (CIDR notation, IPv4 / IPv6)
  - example: `nets = ["172.32.0.0/16"]`

**Built-in defaults**: RFC1918 (`10/8`, `172.16/12`, `192.168/16`) + IPv4/IPv6 loopback + IPv6 ULA (`fc00::/7`). Special addresses such as CGNAT / link-local are not treated as intranet by default; extend via config.

**Example**
```toml
[intranet_nets]
enabled = true
mode = "add"
nets = ["172.32.0.0/16"]
```

### `[provider.sqldb]`

- `name`
  - optional; required when multiple databases, used for OML query prefix routing
  - without a name (or a single `[provider.sqldb]`), the effective name is `default`
  - only `[A-Za-z0-9_]`; duplicates are rejected
- `kind`
  - required, `postgres` or `mysql`
- `connection_uri`
  - required, `${VAR}` expansion supported
- `pool_size`
  - optional, default `8`
- `postgres_session`
  - optional; per-connection session initialization, see next section
  - only valid for `kind = "postgres"`; other kinds are rejected at load time

Do not write `kind = "sqlite_authority"`. In directory-based mode, omit `[provider.sqldb]` completely.

### `[provider.sqldb.postgres_session]`

Per-connection session initialization for PostgreSQL (stabilizes execution plans); only valid with `kind = "postgres"`.

- `plan_cache_mode`
  - optional, `auto` / `force_generic_plan` / `force_custom_plan`
  - requires PostgreSQL 12+; prefer `force_generic_plan` for parameterized lookups such as IP geolocation
- `jit`
  - optional, `true` / `false`
  - prefer `false` for small queries to avoid JIT compile overhead
- `application_name`
  - optional, ≤ 63 bytes, no control characters; single quotes are escaped automatically
  - visible in `pg_stat_activity` for monitoring
- Unset items are not sent, keeping the database default; when the whole block is absent the pool behaves exactly as before

### `[[tables]]`

- `name`
  - required
- `dir`
  - defaults to `name`
- `data_file`
  - defaults to `data.csv`
- `enabled`
  - defaults to `true`
- configure at least one column mapping:
  - `columns.by_header`
  - `columns.by_index`
- priority:
  - `by_header` first
  - `by_index` only when `by_header` is empty

## Table directory convention

A typical table directory looks like this:

```text
knowdb/
  knowdb.toml
  example/
    create.sql
    insert.sql
    data.csv
  zone/
    create.sql
    insert.sql
    clean.sql
    data.scv
```

Rules:

- `create.sql` is required.
- `insert.sql` is required.
- `data.csv` is the default data file.
- `clean.sql` is optional; if omitted, the default cleanup statement is `DELETE FROM {table}`.
- `{table}` inside SQL files is replaced with the current table name.

## Paths and environment variables

- `${VAR}` is expanded before TOML deserialization. Example:

```toml
[provider.sqldb]
kind = "mysql"
connection_uri = "mysql://root:${MYSQL_PASSWORD}@127.0.0.1:3306/demo"
```

- Values come from the `EnvDict` passed into `init_thread_cloned_from_knowdb(..., dict)`.
- `wp-knowledge` does not read process environment variables directly.

Path resolution order:

1. If `knowdb_conf` is relative, resolve it against the caller-provided `root`.
2. Resolve `base_dir` relative to the directory that contains `knowdb.toml`.
3. Resolve `tables[n].dir` relative to `base_dir`.
4. Resolve `tables[n].data_file` relative to the table directory.

## Common failures

- `version` is not `2`
- `create.sql`, `insert.sql`, or the data file is missing
- neither `columns.by_header` nor `columns.by_index` is configured
- `columns.by_header` refers to a missing CSV header
- `encoding` is not `utf-8`
- CSV row parsing fails and `on_error = "fail"`
- imported rows are below `expected_rows.min`
- PostgreSQL / MySQL initialization fails to connect
- `postgres_session` mismatches the database (e.g. `plan_cache_mode` on PostgreSQL 11), or the startup `current_setting` check differs from expectations

## Recommended patterns

- In the directory-based SQLite authority mode, do not add a `[provider.sqldb]` block.
- Put `enabled` under `[[tables]]`, not under `[tables.expected_rows]`.
- Keep `delimiter` to a single character.
- If you use `columns.by_header`, keep `csv.has_header = true`.
- In external PostgreSQL / MySQL mode, treat `[cache]` strictly as `result cache` configuration.
