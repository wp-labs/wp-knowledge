//! KnowDB 定期刷新异步服务（RefreshService）。
//!
//! 分层定位：**宿主（如主引擎 daemon）启动本服务**，服务负责各数据源的
//! 周期更新并**并发通知**；宿主收到事件后自行把数据搬入自己的消费侧缓存
//! （引擎的 ProviderWindow / join 镜像），本模块不感知宿主结构。
//!
//! - 每张表一条规格（`RefreshSpec`），各自独立计时；重载完成即通过事件
//!   通道通知（`RefreshEvent{ name, rows }`），互不阻塞。
//! - 事件载荷为 knowdb 原生行 `Vec<RowData>`（列 → 值，类型由数据源/DDL
//!   决定），宿主在边界按需转换 —— 本服务不依赖任何引擎行类型。
//! - 刷新失败（源不可读 / 装载错误）→ 记 warn 并跳过本次，等待下一周期；
//!   事件通道满 → 丢弃本次事件（不阻塞刷新周期），宿主关闭 → 服务退出。
//! - 首次 tick 不立即重载（宿主已做启动装载），从首个 `interval` 到期开始。
//!
//! 数据源 v1：
//! - [`RefreshSource::Authority`]：KnowDB V2 conf + sqlite 权威库文件单表
//!   重载（CSV 重读 → 建表/清空/重灌 → 投影行；见 `loader::reload_table_rows`）。
//! - [`RefreshSource::NamedSql`]：命名 SQL provider 直查重载
//!   （`facade::query_async_for`；PG/MySQL 供给，异步池查询）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::error::{KnowReason, KnowledgeResult};
use crate::mem::RowData;
use orion_error::conversion::ToStructError;

/// 刷新变量提供器（宿主注入）：每次执行 SQL 前现算 `$name → 值` 列表。
///
/// 用途：供给查询需要随**当前时刻**变化的参数（如相位供给的 `$cur`/`$next`
/// ——当前相位由宿主引擎现算），SQL 模板里写 `$name` 占位符，刷新循环执行前
/// 用返回值做文本替换。语义完全归宿主；本模块只提供"取数 + 替换"两个小能力。
#[derive(Clone)]
pub struct RefreshVars(Arc<dyn Fn() -> KnowledgeResult<Vec<(String, String)>> + Send + Sync>);

impl RefreshVars {
    /// 以变量计算函数构造（每 tick 调用一次）。
    pub fn new(
        f: impl Fn() -> KnowledgeResult<Vec<(String, String)>> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(f))
    }

    /// 现算本次变量（`(name, value)`；替换 `$name`）。
    pub fn compute(&self) -> KnowledgeResult<Vec<(String, String)>> {
        (self.0)()
    }
}

impl std::fmt::Debug for RefreshVars {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RefreshVars(..)")
    }
}

/// 事件通道容量（满则丢弃单次事件，绝不阻塞刷新周期）。
pub const DEFAULT_EVENT_CAPACITY: usize = 1024;

/// 数据源刷新方式。
#[derive(Debug, Clone)]
pub enum RefreshSource {
    /// KnowDB V2 权威库单表重载。`authority_uri` = sqlite 权威库文件 URI；
    /// 每次重载重读 CSV → 重灌该表 → 返回全表行（列类型由表目录 DDL 决定）。
    Authority {
        /// KnowDB conf 所在目录（conf 内 `base_dir` 相对它解析）。
        root: PathBuf,
        /// KnowDB V2 conf 路径（相对 root 或绝对路径均可）。
        conf: PathBuf,
        /// sqlite 权威库文件 URI（如 `file:/tmp/x.sqlite`）。
        authority_uri: String,
        /// 要刷新装载的表名（须在 conf 启用且带 `columns.by_header`）。
        table: String,
    },
    /// 命名 SQL provider 直查（`facade::query_async_for`）。
    NamedSql {
        /// 已注册的 provider 名（`init_*_provider_named`）。
        provider: String,
        /// 每次刷新执行的 SQL（模板：可含 `$name` 占位符，由 [`RefreshVars`] 替换）。
        sql: String,
        /// 变量计算（宿主）：`Some` 时每次执行前调用并替换 SQL 中的 `$name`；
        /// `None` = 静态 SQL 直接执行。
        vars: Option<RefreshVars>,
    },
}

/// 把 `$name` 占位符替换为对应值（文本替换；值不应含 `$`）。
/// 供刷新循环与宿主 boot 装载共用（boot 与首 refresh 必须同源渲染）。
pub fn resolve_sql_vars(sql: &str, vars: &[(String, String)]) -> String {
    let mut out = sql.to_string();
    for (name, value) in vars {
        out = out.replace(&format!("${name}"), value);
    }
    out
}

/// 一条定期刷新规格。
#[derive(Debug, Clone)]
pub struct RefreshSpec {
    /// 表/窗名（事件携带，宿主据此搬入目标）。
    pub name: String,
    /// 刷新周期。
    pub interval: Duration,
    /// 数据源。
    pub source: RefreshSource,
}

/// 一次成功重载的产物（原生行）。
#[derive(Debug)]
pub struct RefreshEvent {
    /// 对应 `RefreshSpec::name`。
    pub name: String,
    /// 该表全量行（列 → 值；宿主在边界转换）。
    pub rows: Vec<RowData>,
}

/// 刷新服务句柄：持有事件接收端与各表任务（drop 时中止全部任务）。
pub struct RefreshService {
    /// 事件接收端（每表独立计时、完成即通知，顺序不保证）。
    pub events: mpsc::Receiver<RefreshEvent>,
    handles: Vec<tokio::task::AbortHandle>,
}

impl RefreshService {
    /// 以给定规格启动服务。**需在 tokio runtime 内调用**（内部 `tokio::spawn`）；
    /// 空规格 = 无任务、通道即闭。宿主持有本句柄：drop / [`Self::shutdown`] 会
    /// 中止全部刷新任务（事件通道随之关闭），可作退出清理。
    pub fn spawn(specs: Vec<RefreshSpec>) -> Self {
        let (tx, events) = mpsc::channel(DEFAULT_EVENT_CAPACITY);
        let mut handles = Vec::with_capacity(specs.len());
        for spec in specs {
            let tx = tx.clone();
            handles.push(tokio::spawn(run_spec(spec, tx)).abort_handle());
        }
        drop(tx);
        Self { events, handles }
    }

    /// 中止全部刷新任务（等价于 drop，通道随后关闭）。
    pub fn shutdown(&mut self) {
        for h in self.handles.drain(..) {
            h.abort();
        }
    }
}

impl Drop for RefreshService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn run_spec(spec: RefreshSpec, tx: mpsc::Sender<RefreshEvent>) {
    if spec.interval.is_zero() {
        log::warn!("refresh spec {:?} interval is zero; skipped", spec.name);
        return;
    }
    let mut ticker = tokio::time::interval(spec.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // tokio interval 首 tick 立即到期 —— 跳过（启动装载由宿主完成）。
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let rows = match reload(&spec).await {
            Ok(rows) => rows,
            Err(e) => {
                log::warn!("knowdb refresh {:?} reload failed: {e}", spec.name);
                continue;
            }
        };
        if let Err(e) = tx.try_send(RefreshEvent {
            name: spec.name.clone(),
            rows,
        }) {
            match e {
                mpsc::error::TrySendError::Full(_) => {
                    log::warn!(
                        "knowdb refresh {:?} event dropped (channel full)",
                        spec.name
                    );
                }
                mpsc::error::TrySendError::Closed(_) => {
                    log::debug!("knowdb refresh {:?} receiver closed; exit", spec.name);
                    return;
                }
            }
        }
    }
}

async fn reload(spec: &RefreshSpec) -> KnowledgeResult<Vec<RowData>> {
    match &spec.source {
        RefreshSource::NamedSql {
            provider,
            sql,
            vars,
        } => {
            // 变量注入：每次执行前现算并替换 `$name`（如 $cur/$next——当前相位
            // 宿主现算）；无 vars = 静态 SQL 直接执行。
            let sql = match vars {
                Some(v) => resolve_sql_vars(sql, &v.compute()?),
                None => sql.clone(),
            };
            crate::facade::query_async_for(provider, &sql).await
        }
        RefreshSource::Authority {
            root,
            conf,
            authority_uri,
            table,
        } => {
            let root = root.clone();
            let conf = conf.clone();
            let authority_uri = authority_uri.clone();
            let table = table.clone();
            tokio::task::spawn_blocking(move || {
                crate::loader::reload_table_rows(
                    &root,
                    &conf,
                    &authority_uri,
                    &table,
                    &orion_variate::EnvDict::default(),
                )
            })
            .await
            .map_err(|join| {
                KnowReason::from_res()
                    .to_err()
                    .with_detail(format!("refresh task join failed: {join}"))
            })?
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;

    fn fixture_spec(table: &str, tag: &str) -> RefreshSpec {
        RefreshSpec {
            name: table.to_string(),
            interval: Duration::from_millis(80),
            source: RefreshSource::Authority {
                root: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("knowdb"),
                conf: PathBuf::from("knowdb.toml"),
                authority_uri: format!(
                    "file:{}/refresh_fixture_{}_{}_{}.sqlite",
                    std::env::temp_dir().display(),
                    table,
                    tag,
                    std::process::id()
                ),
                table: table.to_string(),
            },
        }
    }

    async fn collect(
        service: &mut RefreshService,
        n: usize,
        timeout: Duration,
    ) -> Vec<RefreshEvent> {
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;
        while out.len() < n {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, service.events.recv()).await {
                Ok(Some(ev)) => out.push(ev),
                _ => break,
            }
        }
        out
    }

    #[tokio::test]
    async fn authority_source_fires_periodic_events_with_typed_rows() {
        let mut service = RefreshService::spawn(vec![fixture_spec("address", "a")]);
        let events = collect(&mut service, 2, Duration::from_millis(1500)).await;
        assert!(
            events.len() >= 2,
            "期望 ≥2 次周期事件，实际 {}",
            events.len()
        );
        for ev in &events {
            assert_eq!(ev.name, "address");
            assert_eq!(ev.rows.len(), 10, "address 表应重灌 10 行");
            let field = &ev.rows[0][0];
            assert_eq!(field.get_name(), "value");
        }
    }

    #[tokio::test]
    async fn multiple_specs_notify_concurrently_and_independently() {
        let mut service = RefreshService::spawn(vec![
            fixture_spec("address", "m1"),
            fixture_spec("example", "m2"),
        ]);
        let events = collect(&mut service, 4, Duration::from_millis(2000)).await;
        let mut names: Vec<String> = events.iter().map(|e| e.name.clone()).collect();
        names.sort();
        names.dedup();
        assert!(
            names.contains(&"address".to_string()) && names.contains(&"example".to_string()),
            "两表应各自独立出事件: {names:?}"
        );
        assert!(events.len() >= 4, "期望 ≥4 次事件，实际 {}", events.len());
    }

    #[tokio::test]
    async fn zero_interval_spec_is_skipped() {
        let mut spec = fixture_spec("address", "z");
        spec.interval = Duration::ZERO;
        let mut service = RefreshService::spawn(vec![spec]);
        let events = collect(&mut service, 1, Duration::from_millis(200)).await;
        assert!(events.is_empty(), "零周期规格不应出事件");
    }

    #[tokio::test]
    async fn first_event_not_before_first_interval() {
        // 首 tick（立即到期）被跳过：宿主已做启动装载；首事件应出现在
        // interval 之后而非启动瞬间。
        let mut spec = fixture_spec("address", "skip1");
        spec.interval = Duration::from_millis(250);
        let mut service = RefreshService::spawn(vec![spec]);

        let early = collect(&mut service, 1, Duration::from_millis(150)).await;
        assert!(early.is_empty(), "首 interval 前不应出事件");

        let events = collect(&mut service, 1, Duration::from_millis(800)).await;
        assert_eq!(events.len(), 1, "首个 interval 后应恰好出 1 次事件");
        assert_eq!(events[0].name, "address");
        assert_eq!(events[0].rows.len(), 10);
    }

    #[tokio::test]
    async fn reload_failure_is_skipped_and_shutdown_closes_channel() {
        // 未知表 → 每周期重载失败：只 warn 跳过（不 panic、服务存活）；
        // shutdown() 中止任务 → 事件通道关闭（recv 返回 None）。
        let mut spec = fixture_spec("ghost_table", "f");
        spec.interval = Duration::from_millis(60);
        let mut service = RefreshService::spawn(vec![spec]);

        let events = collect(&mut service, 1, Duration::from_millis(400)).await;
        assert!(events.is_empty(), "失败表不应出事件");

        service.shutdown();
        match tokio::time::timeout(Duration::from_millis(300), service.events.recv()).await {
            Ok(None) => {}
            other => panic!("shutdown 后事件通道应关闭并 recv None，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn drop_aborts_tasks_and_closes_channel() {
        let mut service = RefreshService::spawn(vec![fixture_spec("address", "d")]);
        // 至少看到一次成功事件，再 drop 服务（中止任务 + 关通道）。
        let events = collect(&mut service, 1, Duration::from_millis(1500)).await;
        assert_eq!(events.len(), 1);
        drop(service);
        // drop 后无任务可再发事件：这里主要验证不 panic、干净退出（类型层面
        // receiver 已随 service 一并 drop）。
    }

    #[test]
    fn resolve_sql_vars_replaces_placeholders_and_keeps_unknown() {
        let vars = vec![
            ("cur".to_string(), "p7".to_string()),
            ("next".to_string(), "p8".to_string()),
            ("max_age".to_string(), "30 days".to_string()),
        ];
        let sql = "SELECT * FROM t WHERE phase_bucket IN ('$cur','$next') AND win_start >= now() - interval '$max_age'";
        let out = resolve_sql_vars(sql, &vars);
        assert_eq!(
            out,
            "SELECT * FROM t WHERE phase_bucket IN ('p7','p8') AND win_start >= now() - interval '30 days'"
        );
        // 未提供/未用到的占位符保持原样；空变量表 = 原样返回。
        assert_eq!(
            resolve_sql_vars("SELECT * FROM t WHERE k = '$ghost'", &vars),
            "SELECT * FROM t WHERE k = '$ghost'"
        );
        assert_eq!(resolve_sql_vars(sql, &[]), sql);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn named_sql_spec_substitutes_vars_before_each_query() {
        let _guard = crate::runtime::runtime_test_guard().lock_async().await;
        // 准备内存 provider（默认名）：两行 k=a/b，刷新查询按 $cur 过滤。
        let db = crate::mem::memdb::MemDB::instance();
        db.execute("CREATE TABLE refresh_vars_t (k TEXT, v TEXT)")
            .expect("create");
        db.execute("INSERT INTO refresh_vars_t VALUES ('a', '1'), ('b', '2')")
            .expect("seed");
        crate::facade::init_mem_provider(db).expect("init mem provider");
        let vars = RefreshVars::new(|| {
            Ok(vec![
                ("cur".to_string(), "b".to_string()),
                ("next".to_string(), "c".to_string()),
            ])
        });
        let mut service = RefreshService::spawn(vec![RefreshSpec {
            name: "vars_t".into(),
            interval: Duration::from_millis(80),
            source: RefreshSource::NamedSql {
                provider: "default".to_string(),
                sql: "SELECT v FROM refresh_vars_t WHERE k = '$cur'".to_string(),
                vars: Some(vars),
            },
        }]);
        let events = collect(&mut service, 1, Duration::from_millis(1500)).await;
        assert_eq!(events.len(), 1, "应出 1 次刷新事件");
        assert_eq!(events[0].name, "vars_t");
        assert_eq!(events[0].rows.len(), 1, "$cur→'b' 过滤后应只回 1 行");
        let field = &events[0].rows[0][0];
        assert_eq!(field.get_name(), "v");
        assert_eq!(field.to_string(), "chars(2)");
    }
}
