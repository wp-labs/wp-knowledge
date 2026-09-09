# KnowDB 定期刷新与 VEL

[English](../en/guides/refresh.md)

## 分层定位

宿主（如主引擎 daemon）**启动** `RefreshService`；
knowdb 负责各数据源的**周期更新并并发通知**，宿主收到事件后自行把数据搬入自己的
消费侧缓存（如引擎 ProviderWindow / join 镜像）。本服务不感知宿主结构：

- 每张表一条规格（`RefreshSpec`），各自独立计时；
  重载完成即经事件通道发出 `RefreshEvent`
  `{ name, rows }`（原生行 `Vec<RowData>`，宿主在边界自行转换）；
- 刷新失败 → warn 并跳过本次，等待下一周期；事件通道满 → 丢弃本次（不阻塞周期）；
  宿主 drop / `shutdown()` 服务 → 中止全部任务并关闭通道；
- **首 tick 不立即重载**（宿主已做启动装载），从首个 interval 到期开始。

## 数据源

| 源 | 说明 |
|---|---|
| `RefreshSource::Authority` | KnowDB V2 conf + sqlite 权威库单表重载（CSV 重读 → 重灌 → 类型化投影行，同启动装载路径；`loader::reload_table_rows`） |
| `RefreshSource::NamedSql` | 命名 SQL provider 直查（`facade::query_async_for`；PG/MySQL 异步池）。`sql` 可含 `$name` 占位符，由 `code`（**VEL**）每次执行前求值替换 |

## VEL —— 变量求值语言

`crate::vel`：刷新 SQL 的 `$name` 占位符由一小段 VEL 代码每 tick 求值。

语法（每行一个赋值；空行与 `#` 注释忽略，表达式后可带行尾 `#` 注释）：

```text
$name = "字符串字面量"            # 值原样透传（可含 #）
$name = 内建函数(参数)            # 表达式后只允许行尾注释
```

- 变量名：`[A-Za-z_][A-Za-z0-9_]*`；**重复定义**、非法名 → 配置错误；
- 内建（参数为**秒**；标签 prefix 默认 `"p"`）：

| 函数 | 值 |
|---|---|
| `phase_now(period_s, bucket_s[, prefix])` | 当前相位格标签 `prefix + fold(now)`，`fold(t)=(t mod period) div bucket`（epoch、无时区） |
| `phase_next(period_s, bucket_s[, prefix])` | 下一相位格标签 `fold(now + bucket)`（周期末回绕首格） |

求值时钟 = knowdb 自身 tick 墙钟（`vel::current_wall_nanos`）；空 `code` = 静态 SQL
直接执行。渲染为**标识符感知替换**：只匹配完整 `$name`，`$cur` 不会误伤 `$cur2`，
未知 `$...` 原样保留（值不应含 `$`）。

示例（周期性基线只取当前/下一相位格在保留期内的数据）：

```text
$max_age = "30 days"
$cur  = phase_now(240, 15)
$next = phase_next(240, 15)
```

错误语义：解析/求值错误 = 配置错误（错误带行号）；**host 在启动装载（boot）时建议
用同一 `vel::render` 预渲染一次并 fail-fast**，避免运行期才发现；运行期求值失败 →
本次跳过（warn）。

## 宿主接入（机制到代码）

一次接入分三步，且**所有“知道目标是谁、行怎么转”的逻辑都在宿主侧**：

### 0. 一表一 spec 的完整生命周期

```text
宿主 bootstrap                knowdb(RefreshService)                宿主 daemon
    │                               │                                   │
    │ 1. 登记 RefreshSpec ─────────▶│                                   │
    │ 2. 启动装载（自己查一次）       │   每表独立计时（interval）           │
    │                               │   ├─ 首 tick：跳过                  │
    │                               │   ├─ tick: reload(spec)            │
    │                               │   │    ├─ NamedSql: VEL 求值替换      │
    │                               │   │    │  sql → query_async_for      │
    │                               │   │    └─ Authority: 重读 CSV 重灌     │
    │                               │   └─ 成功 → 事件 {name, rows} ──────▶│
    │                               │        失败 → warn，等下一周期          │ 3. 消费事件：
    │                               │                                   │    按 name 定位目标，
    │                               │                                   │    转换 rows 并搬入
```

### 1. bootstrap：构造 spec 并登记（可先 fail-fast 预渲染一次）

> 下面代码里的 `register_spec` / `take_registered_specs` / `lookup_target` /
> `convert_row` 是**宿主自己的胶水**——wp_knowledge 只提供 `RefreshSpec`、
> `RefreshService`、`vel::render` 与事件本体；引擎的真实写法见文末“真实宿主对照”。

```rust
use wp_knowledge::refresh::{RefreshSource, RefreshSpec};

// NamedSql + VEL：sql 模板含 $name，code 是 VEL 文本（每 tick 由 knowdb 现算）
let sql = "SELECT * FROM facts WHERE slot IN ('$cur','$next') AND ts >= now() - interval '$max_age'";
let code = r#"
$max_age = "30 days"
$cur  = phase_now(240, 15)
$next = phase_next(240, 15)
"#;

// 启动装载：宿主自己先查一次（同一渲染、同源）——VEL 配置错在这里就暴露
let boot_sql = wp_knowledge::vel::render(sql, code, wp_knowledge::vel::current_wall_nanos())
    .expect("VEL 配置错误应 fail-fast");
let rows = wp_knowledge::facade::query_for("engine_pg", &boot_sql)?;
// 转成宿主行并填入你的目标表/join 窗（启动装载）……

// 登记：interval 到期后由 RefreshService 周期重跑同一查询
register_spec(RefreshSpec {
    name: "baseline_ref".into(),          // 事件里带这个名字，宿主据此定位目标
    interval: std::time::Duration::from_secs(1),
    source: RefreshSource::NamedSql {
        provider: "engine_pg".into(),     // facade::init_postgres_provider_named_uri 注册过的名字
        sql: sql.into(),
        code: code.into(),
    },
});

// Authority 形态（CSV 重灌）则不需要 VEL：
// RefreshSource::Authority { root, conf, authority_uri, table }
```

### 2. daemon：启动服务并消费事件（搬数据在你这里）

```rust
use tokio_util::sync::CancellationToken;
use wp_knowledge::refresh::RefreshService;

let mut service = RefreshService::spawn(take_registered_specs()); // 空 spec = 无任务、通道即闭
loop {
    tokio::select! {
        _ = cancel.cancelled() => break,
        ev = service.events.recv() => match ev {
            Some(event) => apply(event.name.as_str(), event.rows), // ← 宿主搬入
            None => break,   // 全部任务结束 / 服务被 shutdown → 通道关闭
        }
    }
}

// apply：原生行 → 宿主行 → 覆盖目标（引擎侧 = engine_rows_from_knowdb + Window::load）
fn apply(name: &str, rows: Vec<wp_knowledge::mem::RowData>) {
    let target = lookup_target(name);            // 你按 spec.name 维护的目标
    let mine = rows.into_iter().map(convert_row).collect();
    target.replace_all(mine);                    // 整表换行 + 重建索引
}
```

### 3. 退出

- 任务侧：`service.shutdown()` 中止全部 spec 任务（drop 等价）；事件通道随之关闭，
  消费循环 `recv()` 返回 `None` 退出；
- 建议配合 `CancellationToken`（如上）让消费循环与引擎主循环一起收尾。

### 边界语义（务必记住）

- **首 tick 不重载**：启动装载是你自己做的（步骤 1 已查一次）——别在 bootstrap 后
  立即期待事件；
- **每 tick 独立**：一表失败只跳过自己（warn），不阻塞其它表；
- **事件可能被丢弃**：通道满时丢本次事件（不阻塞周期）——你的消费端应能容忍缺一
  次刷新，以自身周期重查/下次事件自愈；
- **只搬全表结果**：每次事件是“该表全量行”，语义 = 整表替换，不是增量。

### 真实宿主对照

- 引擎（wf-runtime `lifecycle/provider_refresh.rs`）：登记 spec 静态库 → daemon 启动
  RefreshService → `apply_event` 把原生行经 `engine_rows_from_knowdb` 转引擎行后
  `ProviderWindow::load()` 整表替换并重建 join 索引；
- 完整可跑示例：`wf-examples/baseline` 的 PG 供给（`knowdb.pg.toml` 的 `code` 块 +
  boot 装载/1s 刷新/`run.sh --pg` 全链）。
