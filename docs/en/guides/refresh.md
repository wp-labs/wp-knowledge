# KnowDB Periodic Refresh & VEL

[中文](../zh/guides/refresh.md)

## Layering

The **host** (e.g. an engine daemon) starts the `RefreshService`;
KnowDB owns per-source **periodic updates and concurrent notifications**, and the host
moves rows into its own consumer-side cache (e.g. engine ProviderWindow / join mirror)
on events. This module is agnostic of host structure:

- One `RefreshSpec` per table, each timed independently;
  a successful reload is published as a `RefreshEvent`
  `{ name, rows }` (native rows `Vec<RowData>`; conversion happens at the host boundary).
- Failed reload → warn and skip this tick (next period retries); full event channel →
  drop this event (never blocks the cycle); host drop / `shutdown()` aborts all tasks
  and closes the channel.
- The **first tick is skipped** (the host already loaded at startup); events start after
  the first interval elapses.

## Sources

| Source | Notes |
|---|---|
| `RefreshSource::Authority` | KnowDB V2 conf + sqlite authority single-table reload (re-read CSV → recreate/clean/insert → typed projection rows; same path as startup loading via `loader::reload_table_rows`) |
| `RefreshSource::NamedSql` | Named SQL provider query (`facade::query_async_for`; PG/MySQL async pool). `sql` may contain `$name` placeholders substituted before each execution from `code` (**VEL**) |

## VEL — Variable Evaluation Language

`crate::vel`: a tiny assignment DSL evaluated each tick to feed `$name` placeholders.

Grammar (one assignment per line; blank lines and `#` comments ignored; inline `#`
comments allowed after an expression):

```text
$name = "string literal"          # value passed through as-is (may contain #)
$name = builtin(args)             # only an inline # comment may follow
```

- Names: `[A-Za-z_][A-Za-z0-9_]*`; **duplicate definitions** / invalid names are config
  errors.
- Builtins (arguments in **seconds**; label prefix defaults to `"p"`):

| Function | Value |
|---|---|
| `phase_now(period_s, bucket_s[, prefix])` | Current phase-slot label `prefix + fold(now)`, `fold(t)=(t mod period) div bucket` (epoch, timezone-free) |
| `phase_next(period_s, bucket_s[, prefix])` | Next phase-slot label `fold(now + bucket)` (wraps to slot 0 at period end) |

The clock is KnowDB's own tick wall clock (`vel::current_wall_nanos`). Empty `code`
executes the SQL verbatim. Rendering is **identifier-aware**: only a full `$name`
matches — `$cur` never corrupts `$cur2`, unknown `$...` stays as-is (values must not
contain `$`).

Example (periodic baseline over the current/next phase slots within a retention):

```text
$max_age = "30 days"
$cur  = phase_now(240, 15)
$next = phase_next(240, 15)
```

Error semantics: parse/eval errors are configuration errors (reported with line
numbers). Hosts should pre-render once at bootstrap with the same `vel::render` and
**fail fast**; a runtime eval failure skips that tick (warn).

## Host integration

1. Bootstrap: register `RefreshSpec`s (see wf-runtime `lifecycle/provider_refresh` for a
   reference usage); NamedSql `code` is VEL text, and the boot load renders with the
   same function as refreshes.
2. Daemon: `RefreshService::spawn(specs)` then consume `service.events.recv()`, convert
   `RefreshEvent.rows`, and move them into the target.
3. Shutdown: drop / `shutdown()` for a clean stop.

A complete working example lives in `wf-examples/baseline` (PG supply: the `code` block
in `knowdb.pg.toml` plus engine boot load / 1s refresh).
