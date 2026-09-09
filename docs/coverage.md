# 单元测试覆盖率（Snapshot）

> 数据快照，非 CI 实时结果。更新方法见文末「重新生成」。

| 项目 | 值 |
|---|---|
| 采集日期 | 2026-09-09 |
| 工具 | `cargo-llvm-cov` 0.6.16 |
| 工具链 | rustc 1.97.1（stable-aarch64-apple-darwin） |
| 采集范围 | `cargo llvm-cov --lib --summary-only`（默认特性，src 单测；外部 DB 的 ignored 集成测试未计入） |

## 总计

| Lines | Functions | Regions |
|---|---|---|
| **65.35%**（覆盖 6,674 / 10,212） | 63.53%（737 / 1,160） | 63.58%（10,762 / 16,928） |

> 行覆盖精确值：10,212 行，覆盖 6,674，missed 3,538。

## 重点模块

| 模块 | Lines 覆盖 | 说明 |
|---|---|---|
| `refresh.rs`（RefreshService/TableStore/VEL 交付） | **97.34%** | 换代-信号-函数取数重构后补齐的测试 |
| `vel.rs`（变量求值语言） | **95.86%** | 含相位内建/标识符边界替换等用例 |
| `loader.rs`（权威库装载/重灌/投影） | 94.55% | 装载主路径与错误路径 |
| `sql_route.rs` / `provider_runtime.rs` / `intranet_nets.rs` | 74–89% | 路由与运行时 |
| `postgres.rs` / `mysql.rs` / `redis.rs` | 14–43% | **需外部 DB / redis 的 ignored 集成测试**（`-- --ignored`），非单测缺口 |
| `cache_util.rs` / `mem/stub.rs` 等 | 0% | 启动/兼容分支或外部桩，属预期盲区 |

> 说明：provider 类文件覆盖低是**测试分层**所致——连通性/正确性/性能在
> `tests/*_provider*` 集成层验证（需 `docker compose` + 环境变量，见 README「测试外部 Provider」），
> 不计入 `--lib` 单测统计。

## 重新生成

```bash
cargo llvm-cov --lib --summary-only        # 单测覆盖（本文快照口径）
cargo llvm-cov --all-features --html       # 全特性 HTML 报告（含需 DB 的测试须先设连接串）
```

生成后同步更新本文件「总计/重点模块」与 README 徽章数值。
