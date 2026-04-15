# KnowDB Configuration

[中文](../../zh/guides/config.md) | English

This document answers four questions:

1. How to write `knowdb.toml`.
2. Which settings are effective in which mode.
3. How paths and `${VAR}` are resolved.
4. Which errors fail initialization immediately.

Only `version = 2` is supported today.

## Two modes

`wp-knowledge` has two configuration modes:

- Directory-based SQLite authority
  - `[[tables]]` defines table directories, SQL files, and CSV data.
  - Initialization first builds the authority SQLite database, then switches to the read-only query provider.
- External provider
  - `[provider]` connects directly to PostgreSQL or MySQL.
  - No local authority SQLite database is built.

How to choose:

- If your data ships as static CSV files, use the directory-based SQLite authority mode.
- If your data already lives in PostgreSQL or MySQL, use the external provider mode.

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

```toml
version = 2

[cache]
enabled = true
capacity = 1024
ttl_ms = 30000

[provider]
kind = "postgres"
connection_uri = "postgres://user:${DB_PASSWORD}@127.0.0.1:5432/demo"
pool_size = 8
```

Replace `kind = "postgres"` with `kind = "mysql"` and switch the connection URI to MySQL format if needed.

## Which settings take effect

### Directory-based SQLite authority mode

Effective:

- `version`
- `base_dir`
- `[default]`
- `[csv]`
- `[cache]`
- `[[tables]]`

Not read from `knowdb.toml`:

- `authority_uri`

`authority_uri` is supplied separately by the host when calling `facade::init_thread_cloned_from_knowdb(...)`.

### External PostgreSQL / MySQL mode

Effective:

- `version`
- `[provider]`
- `[cache]`

Still parsed but not used by the import pipeline:

- `base_dir`
- `[default]`
- `[csv]`
- `[[tables]]`

## Field quick reference

### Top-level fields

| Field | Required | Default | Description |
| --- | --- | --- | --- |
| `version` | Yes | None | Must currently be `2` |
| `base_dir` | No | `"."` | Root directory for table folders, resolved relative to the directory that contains `knowdb.toml` |
| `provider` | No | None | Configures an external PostgreSQL / MySQL provider |
| `tables` | No | `[]` | Table definitions for the directory-based SQLite authority mode |

### `[default]`

Only affects the directory-based SQLite authority import flow.

| Field | Default | Description |
| --- | --- | --- |
| `transaction` | `true` | Import rows in batches under transactions |
| `batch_size` | `2000` | Rows committed per batch; the effective minimum is `1` |
| `on_error` | `"fail"` | `fail` stops immediately, `skip` ignores bad rows |

### `[csv]`

Only affects CSV reading in the directory-based SQLite authority mode.

| Field | Default | Description |
| --- | --- | --- |
| `has_header` | `true` | Whether the first row is a header |
| `delimiter` | `","` | Column delimiter; keep it to a single character |
| `encoding` | `"utf-8"` | Only `utf-8` is currently supported |
| `trim` | `true` | Trim surrounding whitespace during reads |

### `[cache]`

Only controls the runtime `result cache`. It does not affect `local cache` or `metadata cache`.

| Field | Default | Description |
| --- | --- | --- |
| `enabled` | `true` | If disabled, the global result cache degenerates to `Bypass` |
| `capacity` | `1024` | Maximum cached entries, not bytes |
| `ttl_ms` | `30000` | Cache TTL in milliseconds |

### `[provider]`

Only used in external provider mode.

| Field | Required | Default | Description |
| --- | --- | --- | --- |
| `kind` | Yes | None | `postgres` or `mysql` |
| `connection_uri` | Yes | None | Database connection string, `${VAR}` expansion is supported |
| `pool_size` | No | `8` | Connection pool size; `0` also falls back to `8` |

Notes:

- Do not write `kind = "sqlite_authority"` in `knowdb.toml`.
- For the directory-based SQLite authority mode, simply omit the whole `[provider]` block.

### `[[tables]]`

Only used in the directory-based SQLite authority mode.

| Field | Required | Default | Description |
| --- | --- | --- | --- |
| `name` | Yes | None | Logical table name and the replacement value for `{table}` |
| `dir` | No | Same as `name` | Table directory, resolved relative to `base_dir` |
| `data_file` | No | `"data.csv"` | Data file, resolved relative to the table directory |
| `enabled` | No | `true` | Whether to import the table |

At least one column mapping must be configured:

- `columns.by_header = ["col1", "col2"]`
- `columns.by_index = [0, 1]`

Priority:

- `by_header` wins when it is not empty.
- `by_index` is only used when `by_header` is empty.

`[tables.expected_rows]` is optional:

| Field | Description |
| --- | --- |
| `min` | Fail immediately when the imported row count is below this value |
| `max` | Warn, but do not fail, when the imported row count exceeds this value |

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
- `data.csv` is the default data file; you can point `data_file` at a different filename.
- `clean.sql` is optional; if omitted, the default cleanup statement is `DELETE FROM {table}`.
- `{table}` inside SQL files is replaced with the current table name.

## Paths and environment variables

### How `${VAR}` is expanded

`knowdb.toml` goes through `${VAR}` expansion before TOML deserialization. Example:

```toml
[provider]
kind = "mysql"
connection_uri = "mysql://root:${MYSQL_PASSWORD}@127.0.0.1:3306/demo"
```

Important:

- Values come from the `EnvDict` passed into `init_thread_cloned_from_knowdb(..., dict)`.
- `wp-knowledge` does not read process environment variables directly.

### How paths are resolved

Path resolution order:

1. If `knowdb_conf` is relative, resolve it against the caller-provided `root`.
2. Resolve `base_dir` relative to the directory that contains `knowdb.toml`.
3. Resolve `tables[n].dir` relative to `base_dir`.
4. Resolve `tables[n].data_file` relative to the table directory.

## Common initialization failures

The following fail immediately:

- `version` is not `2`
- `create.sql`, `insert.sql`, or the data file is missing
- neither `columns.by_header` nor `columns.by_index` is configured
- `columns.by_header` refers to a missing CSV header
- `encoding` is not `utf-8`
- CSV row parsing fails and `on_error = "fail"`
- imported rows are below `expected_rows.min`
- PostgreSQL / MySQL initialization fails to connect

The following only warn:

- imported rows exceed `expected_rows.max`
- bad rows are encountered while `on_error = "skip"`

## Recommended patterns

- In the directory-based SQLite authority mode, do not add a `[provider]` block.
- Put `enabled` under `[[tables]]`, not under `[tables.expected_rows]`.
- Keep `delimiter` to a single character.
- If you use `columns.by_header`, keep `csv.has_header = true`.
- In external PostgreSQL / MySQL mode, treat `[cache]` strictly as `result cache` configuration, not as a master switch for every cache layer.
