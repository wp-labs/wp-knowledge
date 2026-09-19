# KnowDB Periodic Refresh & VEL

[中文](../zh/guides/refresh.md)

## Layering

The **host** (e.g. an engine daemon) starts the `RefreshService`;
KnowDB owns per-source **periodic updates** and swaps each table's newest generation into
its own snapshot store ([`TableStore`]); it then sends only a **signal**. The host (caller)
pulls the current generation with a **function call** (`snapshot`) and moves it into its own
consumer-side cache (e.g. engine ProviderWindow / join mirror). This module is agnostic of
host structure:

- One `RefreshSpec` per table, each timed independently; a successful reload builds the
  next `Arc<TableData>` off-lock, `TableStore::insert` swaps it in (O(1) Arc pointer
  replacement — atomic generation swap), then the signal channel sends
  `RefreshSignal{ name }` (**signal only, no rows** — callers fetch by `snapshot`).
- **Duplicate names keep only the first**: registering the same `spec.name` twice (two
  parallel tickers would swap the same table out of order) is deduplicated by
  `spawn`/`spawn_with_store` (warn and skip later duplicates) — one refresh task per
  table.
- Failed reload → warn and skip this tick (next period retries); a full signal channel →
  drop the signal (never blocks the cycle; the data is already swapped in and pullable —
  dropping a signal is harmless); host drop / `shutdown()` aborts all tasks and closes the
  channel.
- The **first tick is skipped** (the caller loaded at startup via `load_rows` or seeded
  itself); signals start after the first interval elapses.

## Sources

| Source | Notes |
|---|---|
| `RefreshSource::Authority` | KnowDB V2 conf + sqlite authority single-table reload (re-read CSV → recreate/clean/insert → typed projection rows; same path as startup loading via `loader::reload_table_rows`) |
| `RefreshSource::NamedSql` | Named SQL provider query (`facade::query_async_for`; PG/MySQL async pool). `sql` may contain `$name` placeholders substituted before each execution from `code` (**VEL**); the synchronous bootstrap load uses the same render via `load_rows` (`facade::query_for`) |

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
numbers). Hosts should call the same sync `load_rows` once at bootstrap and **fail fast**
(a VEL error surfaces there instead of at the first tick); a runtime eval failure skips
that tick (warn).

## Host integration (mechanism → code)

Integration has three steps; **all “which target” and “how to convert rows” logic lives
on the host side**:

### 0. Lifecycle of one table spec

```text
host bootstrap              knowdb (RefreshService + TableStore)            host daemon
      │                                 │                                      │
      │ 1. register RefreshSpec ────────▶│                                      │
      │ 2. bootstrap load (sync load_rows,│  per-table independent timer         │
      │     may seed the same store)     │  ├─ first tick: skipped              │
      │                                  │  ├─ tick: reload(spec)               │
      │                                  │  │   ├─ NamedSql: evaluate VEL       │
      │                                  │  │   │   → query_async_for           │
      │                                  │  │   └─ Authority: reload CSV         │
      │                                  │  └─ ok → store swap + signal {name} ─▶│
      │                                  │      err → warn, next period           │ 3. on signal →
      │                                  │                                      │    snapshot(name)
      │                                  │                                      │    pull current
      │                                  │                                      │    generation (Arc,
      │                                  │                                      │    zero copy) → apply
```

### 1. Bootstrap: build a spec → load synchronously (fail-fast) → register

> In the snippets below, `register_spec` / `take_registered_specs` / `lookup_target` /
> `convert_row` are **host-side glue** — wp-knowledge only provides `RefreshSpec`,
> `RefreshService`, `TableStore`/`TableData`, the synchronous `load_rows`, and the
> signals themselves; see “Reference hosts” for the engine’s real wiring.

```rust
use wp_knowledge::refresh::{RefreshSource, RefreshSpec, TableData, TableStore};

// NamedSql + VEL: the sql template carries $name; code is VEL text (evaluated each tick)
let sql = "SELECT * FROM facts WHERE slot IN ('$cur','$next') AND ts >= now() - interval '$max_age'";
let code = r#"
$max_age = "30 days"
$cur  = phase_now(240, 15)
$next = phase_next(240, 15)
"#;

let spec = RefreshSpec {
    name: "baseline_ref".into(),      // carried on signals; host snapshots by name
    interval: std::time::Duration::from_secs(1),
    source: RefreshSource::NamedSql {
        provider: "engine_pg".into(), // a name registered via init_postgres_provider_named_uri
        sql: sql.into(),
        code: code.into(),
    },
};

// Bootstrap load: sync first rows (load_rows renders VEL + queries internally — the same
// code path as each tick). A VEL error surfaces here (fail fast). Optional: seed this
// generation into the TableStore shared with the daemon for one delivery surface.
let store: Arc<TableStore> = /* your shared snapshot-store handle */;
let rows = wp_knowledge::refresh::load_rows(&spec)?;
store.insert(std::sync::Arc::new(TableData { name: spec.name.clone(), rows }));
// convert rows into your host rows and fill the target table / join window …

// Register: after `interval`, RefreshService re-runs the same query and swaps the store.
register_spec(spec);

// The Authority form (CSV reload) needs no VEL:
// RefreshSource::Authority { root, conf, authority_uri, table }
```

### 2. Daemon: spawn the service and consume signals (applying rows is yours)

```rust
use tokio_util::sync::CancellationToken;
use wp_knowledge::refresh::RefreshService;

// Shared TableStore: the bootstrap seed and the daemon swaps use the same instance
let store = /* your shared snapshot-store handle */;
let mut service = RefreshService::spawn_with_store(take_registered_specs(), store);
loop {
    tokio::select! {
        _ = cancel.cancelled() => break,
        sig = service.signals.recv() => match sig {
            Some(signal) => {
                // The data is in the store (KnowDB already swapped it): pull the current
                // generation by function call (Arc, zero copy).
                let Some(data) = service.store.snapshot(&signal.name) else { continue };
                apply(signal.name.as_str(), &data.rows); // ← host applies
            }
            None => break,  // all tasks done / service shut down → channel closed
        }
    }
}

// apply: native rows (borrowed) → host rows → replace target (engine side =
// engine_rows_from_knowdb + ProviderWindow::rebuilt off-lock + O(1) swap_in)
fn apply(name: &str, rows: &[wp_knowledge::mem::RowData]) {
    let target = lookup_target(name);            // your target kept by spec.name
    let mine = rows.iter().map(convert_row).collect();
    target.replace_all(mine);                    // whole-window swap / index rebuild
}
```

### 3. Shutdown

- Task side: `service.shutdown()` aborts every spec task (dropping does the same); the
  signal channel then closes and the consumer loop exits on `recv() == None`;
- Combine with a `CancellationToken` (as above) so the consumer stops with your main loop.

### Edge semantics (remember these)

- **The first tick never reloads**: bootstrap load is yours (`load_rows` or a self seed) —
  do not expect a signal right after registration;
- **Ticks are independent**: one failing table only skips itself (warn), never blocks
  others;
- **Data lives in the store; signals may be dropped**: the swap has already happened, so a
  pull always returns the newest generation; a full channel drops only the signal (never
  blocks the cycle) — missing one or two is harmless, the next signal / your own polling
  self-heals;
- **Every generation is the whole table**: each store generation carries the table’s full
  rows; the semantic is full-table swap, not incremental;
- **Pulling is zero-copy**: `snapshot` returns an `Arc<TableData>` (immutable generation);
  multiple consumers of one generation share it — no whole-row deep copy.
- **Prepare outside the lock, swap inside** (double-buffer recommended): the O(rows)
  row conversion / index rebuild happens off the consumer-side lock; the lock only does an
  O(1) whole-window swap-in — readers are never blocked by the refresh work and never see a
  torn state (engine impl: `ProviderWindow::rebuilt` + `swap_in`).

### Reference hosts

- Engine (wf-runtime `lifecycle/provider_refresh.rs`): registers specs and seeds a shared
  store → daemon spawns RefreshService and consumes **signals** → `store.snapshot` pulls
  the current generation → `engine_rows_from_knowdb` converts the native rows, builds the
  new window **off-lock** with `ProviderWindow::rebuilt()` (rows + join index /
  pre-materialized rows), then swaps it in with an O(1) `swap_in()` under the write lock;
- Runnable example: `wf-examples/baseline` PG supply (`code` block in `knowdb.pg.toml` +
  bootstrap load / 1s refresh / `run.sh --pg`).
