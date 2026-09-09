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

## Host integration (mechanism → code)

Integration has three steps; **all “which target” and “how to convert rows” logic lives
on the host side**:

### 0. Lifecycle of one table spec

```text
host bootstrap                  knowdb (RefreshService)              host daemon
      │                                 │                               │
      │ 1. register RefreshSpec ────────▶│                               │
      │ 2. bootstrap load (host queries) │  per-table independent timer   │
      │                                  │  ├─ first tick: skipped        │
      │                                  │  ├─ tick: reload(spec)         │
      │                                  │  │   ├─ NamedSql: evaluate VEL  │
      │                                  │  │   │   → query_async_for     │
      │                                  │  │   └─ Authority: reload CSV   │
      │                                  │  └─ ok → event {name, rows} ──▶│
      │                                  │      err → warn, next period    │ 3. consume:
      │                                  │                               │    locate target by name,
      │                                  │                               │    convert rows, apply
```

### 1. Bootstrap: build a spec and register (optionally fail-fast once)

> In the snippets below, `register_spec` / `take_registered_specs` / `lookup_target` /
> `convert_row` are **host-side glue** — wp-knowledge only provides `RefreshSpec`,
> `RefreshService`, `vel::render`, and the events themselves; see “Reference hosts” for
> the engine’s real wiring.

```rust
use wp_knowledge::refresh::{RefreshSource, RefreshSpec};

// NamedSql + VEL: the sql template carries $name; code is VEL text (evaluated each tick)
let sql = "SELECT * FROM facts WHERE slot IN ('$cur','$next') AND ts >= now() - interval '$max_age'";
let code = r#"
$max_age = "30 days"
$cur  = phase_now(240, 15)
$next = phase_next(240, 15)
"#;

// Bootstrap load: the host queries once itself with the same render — a VEL config
// error surfaces here (fail fast) instead of at the first tick.
let boot_sql = wp_knowledge::vel::render(sql, code, wp_knowledge::vel::current_wall_nanos())
    .expect("invalid VEL must fail fast");
let rows = wp_knowledge::facade::query_for("engine_pg", &boot_sql)?;
// convert rows into your host rows and fill the target table / join window …

// Register: after `interval`, RefreshService re-runs the same query periodically.
register_spec(RefreshSpec {
    name: "baseline_ref".into(),          // carried on events; host locates its target
    interval: std::time::Duration::from_secs(1),
    source: RefreshSource::NamedSql {
        provider: "engine_pg".into(),     // a name registered via init_postgres_provider_named_uri
        sql: sql.into(),
        code: code.into(),
    },
});

// The Authority form (CSV reload) needs no VEL:
// RefreshSource::Authority { root, conf, authority_uri, table }
```

### 2. Daemon: spawn the service and consume events (applying rows is yours)

```rust
use tokio_util::sync::CancellationToken;
use wp_knowledge::refresh::RefreshService;

let mut service = RefreshService::spawn(take_registered_specs()); // empty specs = no tasks, closed channel
loop {
    tokio::select! {
        _ = cancel.cancelled() => break,
        ev = service.events.recv() => match ev {
            Some(event) => apply(event.name.as_str(), event.rows), // ← host applies
            None => break,  // all tasks done / service shut down → channel closed
        }
    }
}

// apply: native rows → host rows → replace target
fn apply(name: &str, rows: Vec<wp_knowledge::mem::RowData>) {
    let target = lookup_target(name);            // your target kept by spec.name
    let mine = rows.into_iter().map(convert_row).collect();
    target.replace_all(mine);                    // full-table replace + index rebuild
}
```

### 3. Shutdown

- Task side: `service.shutdown()` aborts every spec task (dropping does the same); the
  event channel then closes and the consumer loop exits on `recv() == None`;
- Combine with a `CancellationToken` (as above) so the consumer stops with your main loop.

### Edge semantics (remember these)

- **The first tick never reloads**: bootstrap load is yours (step 1 already queried) —
  do not expect an event right after registration;
- **Ticks are independent**: one failing table only skips itself (warn), never blocks
  others;
- **Events can be dropped**: a full channel drops the event (never blocks the cycle) —
  your consumer must tolerate a skipped refresh and self-heal on the next tick;
- **Events carry the whole table**: each event is the table’s full rows; the semantic is
  full-table replace, not incremental.

### Reference hosts

- Engine (wf-runtime `lifecycle/provider_refresh.rs`): a static spec store → daemon spawns
  RefreshService → `apply_event` converts native rows via `engine_rows_from_knowdb` and
  `ProviderWindow::load()` (full replace + join index rebuild);
- Runnable example: `wf-examples/baseline` PG supply (`code` block in `knowdb.pg.toml` +
  bootstrap load / 1s refresh / `run.sh --pg`).
