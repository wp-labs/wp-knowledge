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

## 宿主接入要点

1. bootstrap：`refresh::register`（仓库内用法见 wf-runtime `lifecycle/provider_refresh`）
   —— 登记 `RefreshSpec`，NamedSql 的 `code` 用 VEL 文本；boot 装载与刷新**同源
   渲染**；
2. daemon：`RefreshService::spawn(specs)` 后循环 `service.events.recv()`，把
   `RefreshEvent.rows` 转为宿主行并搬入目标；
3. 退出：drop / `shutdown()` 干净收尾。

完整可用示例：`wf-examples/baseline` 的 PG 供给（`knowdb.pg.toml` 的 `code` 块 +
引擎 boot 装载/1s 刷新）。
