# KnowDB 定期刷新与 VEL

[English](../en/guides/refresh.md)

## 分层定位

宿主（如主引擎 daemon）**启动** `RefreshService`；
knowdb 负责各数据源的**周期更新并把最新一代数据换入自己的表快照库**（`TableStore`），
随后只发**信号**；宿主（调用者）收到信号后以**函数调用** pull 当前代快照（`snapshot`），
自行把数据搬入自己的消费侧缓存（如引擎 ProviderWindow / join 镜像）。
本服务不感知宿主结构：

- 每张表一条规格（`RefreshSpec`），各自独立计时；tick 重载成功 → 锁外整建
  `Arc<TableData>` → `TableStore::insert`（O(1) 换 Arc 指针，原子换代）→ 信号通道发
  `RefreshSignal{ name }`（**纯信号，无数据载荷**——调用者以函数 `snapshot` 取数）；
- **同名只保留首个**：重复登记同一 `spec.name`（两个并行 ticker 会乱序覆盖同一表）
  由 `spawn`/`spawn_with_store` 去重（warn 并忽略后续同名），一表一条刷新任务；
- 刷新失败 → warn 并跳过本次，等待下一周期；信号通道满 → 丢本次信号（不阻塞周期，
  数据已换代、随时可 pull，丢信号无害）；宿主 drop / `shutdown()` → 中止全部任务并
  关闭通道；
- **首 tick 不立即重载**（调用者已做启动装载：`load_rows` 同步装载或自行 seed），
  从首个 interval 到期开始。

## 数据源

| 源 | 说明 |
|---|---|
| `RefreshSource::Authority` | KnowDB V2 conf + sqlite 权威库单表重载（CSV 重读 → 重灌 → 类型化投影行，同启动装载路径；`loader::reload_table_rows`） |
| `RefreshSource::NamedSql` | 命名 SQL provider 直查（`facade::query_async_for`；PG/MySQL 异步池）。`sql` 可含 `$name` 占位符，由 `code`（**VEL**）每次执行前求值替换；同步启动装载走同渲染的 `load_rows`（`facade::query_for`） |

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

错误语义：解析/求值错误 = 配置错误（错误带行号）；**host 在启动装载（boot）时用
`load_rows`（内部同一渲染）预查一次并 fail-fast**，避免运行期才发现；运行期求值失败
→ 本次跳过（warn）。

## 宿主接入（机制到代码）

一次接入分三步，且**所有“知道目标是谁、行怎么转”的逻辑都在宿主侧**：

### 0. 一表一 spec 的完整生命周期

```text
宿主 bootstrap               knowdb(RefreshService + TableStore)           宿主 daemon
    │                                  │                                        │
    │ 1. 登记 RefreshSpec ────────────▶│                                        │
    │ 2. 启动装载（同步 load_rows，     │   每表独立计时（interval）               │
    │    数据可 seed 进同一 store）     │   ├─ 首 tick：跳过                     │
    │                                  │   ├─ tick: reload(spec)               │
    │                                  │   │    ├─ NamedSql: VEL 求值替换       │
    │                                  │   │    │  sql → query_async_for        │
    │                                  │   │    └─ Authority: 重读 CSV 重灌      │
    │                                  │   └─ 成功 → store 换代 + 信号 {name} ──▶│
    │                                  │        失败 → warn，等下一周期           │ 3. 收信号 →
    │                                  │                                        │    snapshot(name)
    │                                  │                                        │    pull 当前代（Arc，
    │                                  │                                        │    零数据复制）→ 搬入
```

### 1. bootstrap：构造 spec → 同步装载（fail-fast）→ 登记

> 下面代码里的 `register_spec` / `take_registered_specs` / `lookup_target` /
> `convert_row` 是**宿主自己的胶水**——wp_knowledge 只提供 `RefreshSpec`、
> `RefreshService`、`TableStore`/`TableData`、同步 `load_rows` 与信号本体；引擎的
> 真实写法见文末“真实宿主对照”。

```rust
use wp_knowledge::refresh::{RefreshSource, RefreshSpec, TableData, TableStore};

// NamedSql + VEL：sql 模板含 $name，code 是 VEL 文本（每 tick 由 knowdb 现算）
let sql = "SELECT * FROM facts WHERE slot IN ('$cur','$next') AND ts >= now() - interval '$max_age'";
let code = r#"
$max_age = "30 days"
$cur  = phase_now(240, 15)
$next = phase_next(240, 15)
"#;

let spec = RefreshSpec {
    name: "baseline_ref".into(),      // 信号里带这个名字，宿主据此 snapshot
    interval: std::time::Duration::from_secs(1),
    source: RefreshSource::NamedSql {
        provider: "engine_pg".into(), // facade::init_postgres_provider_named_uri 注册过的名字
        sql: sql.into(),
        code: code.into(),
    },
};

// 启动装载：同步取首批行（load_rows 内部做 VEL 渲染 + 查询，与 tick 同一实现）——
// VEL 配置错在这里就 fail-fast。可选：把该代 seed 进与 daemon 共用的 TableStore，
// 启动与刷新同交付面。
let store: Arc<TableStore> = /* 你的共享快照库句柄 */;
let rows = wp_knowledge::refresh::load_rows(&spec)?;
store.insert(std::sync::Arc::new(TableData { name: spec.name.clone(), rows }));
// 转成宿主行并填入你的目标表/join 窗（启动装载）……

// 登记：interval 到期后由 RefreshService 周期重跑同一查询并换代 store
register_spec(spec);

// Authority 形态（CSV 重灌）则不需要 VEL：
// RefreshSource::Authority { root, conf, authority_uri, table }
```

### 2. daemon：启动服务并消费信号（搬数据在你这里）

```rust
use tokio_util::sync::CancellationToken;
use wp_knowledge::refresh::RefreshService;

// 共享 TableStore：启动 seed 与 daemon 换代用同一实例（同交付面）
let store = /* 你的共享快照库句柄 */;
let mut service = RefreshService::spawn_with_store(take_registered_specs(), store);
loop {
    tokio::select! {
        _ = cancel.cancelled() => break,
        sig = service.signals.recv() => match sig {
            Some(signal) => {
                // 数据在 store（knowdb 已换代）：函数调用 pull 当前代（Arc 零复制）
                let Some(data) = service.store.snapshot(&signal.name) else { continue };
                apply(signal.name.as_str(), &data.rows); // ← 宿主搬入
            }
            None => break,   // 全部任务结束 / 服务被 shutdown → 通道关闭
        }
    }
}

// apply：原生行（借用）→ 宿主行 → 覆盖目标（引擎侧 = engine_rows_from_knowdb +
// ProviderWindow::rebuilt 锁外整建 + swap_in O(1) 换入）
fn apply(name: &str, rows: &[wp_knowledge::mem::RowData]) {
    let target = lookup_target(name);            // 你按 spec.name 维护的目标
    let mine = rows.iter().map(convert_row).collect();
    target.replace_all(mine);                    // 整窗换代/重建索引
}
```

### 3. 退出

- 任务侧：`service.shutdown()` 中止全部 spec 任务（drop 等价）；信号通道随之关闭，
  消费循环 `recv()` 返回 `None` 退出；
- 建议配合 `CancellationToken`（如上）让消费循环与引擎主循环一起收尾。

### 边界语义（务必记住）

- **首 tick 不重载**：启动装载是你自己做的（`load_rows` 或自行 seed）——别在 bootstrap 后
  立即期待信号；
- **每 tick 独立**：一表失败只跳过自己（warn），不阻塞其它表；
- **数据在 store，信号可丢**：换代已发生，pull 永远拿到最新；信号通道满只丢信号
  （不阻塞周期），消费端漏一两次也无妨，下个信号/自行轮询即自愈；
- **只搬全表结果**：store 里每代 = “该表全量行”，语义 = 整表换代，不是增量；
- **取数零复制**：`snapshot` 返回 `Arc<TableData>`（不可变代），同一代多消费者共享，
  绝无整行深拷贝。
- **替换在你的锁外准备、锁内换入**（建议 double-buffer）：O(rows) 的行转换/索引重建
  在消费侧锁外完成，锁内只做 O(1) 整窗换入——读者不被刷新重活阻塞，也永不读到
  中间态（引擎实现：`ProviderWindow::rebuilt` + `swap_in`）。

### 真实宿主对照

- 引擎（wf-runtime `lifecycle/provider_refresh.rs`）：登记 spec + 启动 seed 共享 store →
  daemon 启动 RefreshService 消费**信号** → `store.snapshot` pull 当前代 → 原生行经
  `engine_rows_from_knowdb` 转引擎行后**锁外** `ProviderWindow::rebuilt()` 整建
  （rows + join 索引/预物化行），写锁内仅 `swap_in()` O(1) 整窗换入；
- 完整可跑示例：`wf-examples/baseline` 的 PG 供给（`knowdb.pg.toml` 的 `code` 块 +
  boot 装载/1s 刷新/`run.sh --pg` 全链）。
