//! KnowDB 定期刷新异步服务（RefreshService）——**数据由 knowdb 持有，函数调用交付**。
//!
//! 分层定位：**宿主（如主引擎 daemon）启动本服务**，服务负责各数据源的周期更新并
//! 把最新一代数据**换入服务内的表快照库**（[`TableStore`]），随后只发**信号**；
//! 宿主（调用者）收到信号后**以函数调用 pull** 快照（[`TableStore::snapshot`]），
//! 自行把数据搬入自己的消费侧缓存（引擎的 ProviderWindow / join 镜像）。
//! 本模块不感知宿主结构。
//!
//! 交付模型（统一为函数取数，无"事件带行"）：
//! - **启动**：调用者先做一次装载（`load_rows(&spec)`，同步）并换入 store
//!   （或自用），此后 `snapshot(name)` 即得当前代；
//! - **运行期**：每张表一条规格（`RefreshSpec`），各自独立计时；tick 重载成功 →
//!   锁外整建新一代 `Arc<TableData>` → `TableStore::insert`（O(1) 换 Arc 指针，
//!   原子换代）→ 信号通道通知（`RefreshSignal{ name }`，无数据载荷，互不阻塞）。
//! - **调用者取数** = `store.snapshot(name) -> Option<Arc<TableData>>`：短读锁内
//!   clone 一个 `Arc`，**零数据复制**；拿到的代不可变，可无锁并发读（RCU 语义：
//!   换代不打断持有旧代的调用者）。
//! - 刷新失败（源不可读 / 装载错误）→ 记 warn 并跳过本次，等待下一周期；
//!   信号通道满 → 丢弃本次信号（不阻塞刷新周期；数据已在 store 换代，调用者
//!   随时 pull 到最新，丢信号无害）；宿主关闭 → 服务退出。
//! - 首次 tick 不立即重载（启动装载由调用者完成），从首个 `interval` 到期开始。
//!
//! 数据源：
//! - [`RefreshSource::Authority`]：KnowDB V2 conf + sqlite 权威库文件单表
//!   重载（CSV 重读 → 建表/清空/重灌 → 投影行；见 `loader::reload_table_rows`）。
//! - [`RefreshSource::NamedSql`]：命名 SQL provider 直查重载
//!   （`facade::query_async_for` / `facade::query_for`；PG/MySQL 供给，异步池查询）。
//!   SQL 可含 `$name` 占位符，由表级 **VEL**（变量求值语言，见 [`crate::vel`]）代码
//!   每 tick 求值替换。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::error::{KnowReason, KnowledgeResult};
use crate::mem::RowData;
use orion_error::conversion::ToStructError;

/// 信号通道容量（满则丢弃单次信号，绝不阻塞刷新周期——数据已在 store 换代）。
pub const DEFAULT_SIGNAL_CAPACITY: usize = 1024;

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
    /// 命名 SQL provider 直查（`facade::query_async_for` / `facade::query_for`）。
    NamedSql {
        /// 已注册的 provider 名（`init_*_provider_named`）。
        provider: String,
        /// 每次刷新执行的 SQL（模板：可含 `$name` 占位符，由 VEL（见 [`crate::vel`]）
        /// 每次执行前求值替换）。
        sql: String,
        /// VEL 变量代码（每行 `$name = 字面量/内建函数`；空 = 静态 SQL 直接执行）。
        code: String,
    },
}

/// 一条定期刷新规格。
#[derive(Debug, Clone)]
pub struct RefreshSpec {
    /// 表/窗名（信号携带，宿主据此 pull 对应快照）。
    pub name: String,
    /// 刷新周期。
    pub interval: Duration,
    /// 数据源。
    pub source: RefreshSource,
}

/// 一代数据集快照（**不可变**）：knowdb 换入 store、调用者共享读取。
///
/// 换代 = 新 `Arc` 让位旧 `Arc`；调用者手上已拿到的旧代仍可安全读完（数据不
/// 变），因此 `rows` 在发布后绝不允许被修改。
#[derive(Debug)]
pub struct TableData {
    /// 对应 `RefreshSpec::name`。
    pub name: String,
    /// 该表全量行（列 → 值；类型由数据源/DDL 决定，见 [`crate::mem`]）。
    pub rows: Vec<RowData>,
}

/// 表快照库：表名 → 当前代 `Arc<TableData>`。
///
/// - 读（调用者取数）：`snapshot()` 短读锁 clone 一个 `Arc`——O(1)，无数据复制；
///   之后完全无锁读该代。
/// - 写（刷新换代）：tick 在锁外整建新 `TableData`，`insert()` 只在写锁内做一次
///   O(1) 的 Arc 指针替换——读者不被重活阻塞，永不读到半代数据。
#[derive(Default)]
pub struct TableStore {
    current: RwLock<HashMap<String, Arc<TableData>>>,
}

impl TableStore {
    /// 取某表当前代快照（`None` = 尚未有任何一代装载/刷新成功）。
    pub fn snapshot(&self, name: &str) -> Option<Arc<TableData>> {
        self.current
            .read()
            .expect("table store lock poisoned")
            .get(name)
            .cloned()
    }

    /// 换入新一代（按 `data.name` 覆盖当前代）。调用方负责在锁外整建完成。
    pub fn insert(&self, data: Arc<TableData>) {
        self.current
            .write()
            .expect("table store lock poisoned")
            .insert(data.name.clone(), data);
    }
}

/// 一次成功换代后的通知（**纯信号，无数据载荷**——数据在 [`TableStore`]，调用者
/// 收到信号后用函数调用 [`TableStore::snapshot`] 取当前代）。
#[derive(Debug)]
pub struct RefreshSignal {
    /// 对应 `RefreshSpec::name`。
    pub name: String,
}

/// 刷新服务句柄：持有表快照库、信号接收端与各表任务（drop 时中止全部任务）。
pub struct RefreshService {
    /// 表快照库（当前代数据在此；调用者随时 `snapshot` pull）。
    pub store: Arc<TableStore>,
    /// 信号接收端（每表独立计时、换代完成即通知，顺序不保证）。
    pub signals: mpsc::Receiver<RefreshSignal>,
    handles: Vec<tokio::task::AbortHandle>,
}

impl RefreshService {
    /// 以给定规格启动服务（内部新建空快照库）。**需在 tokio runtime 内调用**
    /// （内部 `tokio::spawn`）；空规格 = 无任务、信号通道即闭。宿主持有本句柄：
    /// drop / [`Self::shutdown`] 会中止全部刷新任务（信号通道随之关闭）。
    pub fn spawn(specs: Vec<RefreshSpec>) -> Self {
        Self::spawn_with_store(specs, Arc::new(TableStore::default()))
    }

    /// 以给定规格与**共享快照库**启动：调用者（如引擎）先在启动期用自己的同步
    /// 装载 seed 同一 store（[`load_rows`] + [`TableStore::insert`]），运行期每次
    /// tick 换代 + 信号都落在这个 store——启动与刷新的数据天然同源、同一交付面。
    /// **同名 spec 只保留首个**（重复登记 = 两个并行 ticker 换代同一表，查询耗时
    /// 差异会让旧代覆盖新代）；空规格 = 无任务、信号通道即闭。
    pub fn spawn_with_store(specs: Vec<RefreshSpec>, store: Arc<TableStore>) -> Self {
        let (tx, signals) = mpsc::channel(DEFAULT_SIGNAL_CAPACITY);
        let mut handles = Vec::with_capacity(specs.len());
        let mut seen = HashSet::new();
        for spec in specs {
            if !seen.insert(spec.name.clone()) {
                log::warn!(
                    "knowdb refresh: 重复规格 {} 已忽略（一表一条刷新任务）",
                    spec.name
                );
                continue;
            }
            let tx = tx.clone();
            let store = Arc::clone(&store);
            handles.push(tokio::spawn(run_spec(spec, store, tx)).abort_handle());
        }
        drop(tx);
        Self {
            store,
            signals,
            handles,
        }
    }

    /// 中止全部刷新任务（等价于 drop，信号通道随后关闭）。
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

/// **同步装载一次**（启动首次取数用；与周期 tick 内部 `reload` 同一渲染/查询语义，
/// 只是走同步 facade，供调用者在启动这一同步上下文里拿首批行）。
pub fn load_rows(spec: &RefreshSpec) -> KnowledgeResult<Vec<RowData>> {
    match &spec.source {
        RefreshSource::NamedSql {
            provider,
            sql,
            code,
        } => {
            // VEL：按当前墙钟求值并替换 `$name`（与 tick 异步路径同一渲染）。
            let sql = crate::vel::render(sql, code, crate::vel::current_wall_nanos())?;
            crate::facade::query_for(provider, &sql)
        }
        RefreshSource::Authority {
            root,
            conf,
            authority_uri,
            table,
        } => crate::loader::reload_table_rows(
            root,
            conf,
            authority_uri,
            table,
            &orion_variate::EnvDict::default(),
        ),
    }
}

async fn run_spec(spec: RefreshSpec, store: Arc<TableStore>, tx: mpsc::Sender<RefreshSignal>) {
    if spec.interval.is_zero() {
        log::warn!("refresh spec {:?} interval is zero; skipped", spec.name);
        return;
    }
    let mut ticker = tokio::time::interval(spec.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // tokio interval 首 tick 立即到期 —— 跳过（启动装载由调用者完成）。
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
        // 换代：锁外整建（已就绪），insert 仅 O(1) 换 Arc 指针——原子换代。
        store.insert(Arc::new(TableData {
            name: spec.name.clone(),
            rows,
        }));
        if let Err(e) = tx.try_send(RefreshSignal {
            name: spec.name.clone(),
        }) {
            match e {
                mpsc::error::TrySendError::Full(_) => {
                    log::warn!(
                        "knowdb refresh {:?} signal dropped (channel full; store 已换代)",
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
            code,
        } => {
            // VEL（变量求值语言）：每次执行前按 knowdb 自身时钟求值并替换 `$name`
            // （如 $cur/$next——见 [`crate::vel`] 内建）。空 = 静态 SQL。
            let sql = crate::vel::render(sql, code, crate::vel::current_wall_nanos())?;
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
    ) -> Vec<RefreshSignal> {
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;
        while out.len() < n {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, service.signals.recv()).await {
                Ok(Some(sig)) => out.push(sig),
                _ => break,
            }
        }
        out
    }

    // -----------------------------------------------------------------------
    // TableStore：Arc 快照换代语义（零复制交付的底座）
    // -----------------------------------------------------------------------

    #[test]
    fn store_snapshot_missing_is_none_and_insert_returns_current_generation() {
        let store = TableStore::default();
        assert!(store.snapshot("nope").is_none(), "无装载 → None");

        let gen1 = Arc::new(TableData {
            name: "t".into(),
            rows: Vec::new(),
        });
        store.insert(Arc::clone(&gen1));
        let snap = store.snapshot("t").expect("装载后应有当前代");
        assert!(Arc::ptr_eq(&snap, &gen1), "snapshot 应零复制共享同一 Arc");
    }

    #[test]
    fn store_swap_keeps_old_generation_valid_for_holders() {
        // RCU 语义：换代只替换 store 里的当前代；已拿 Arc 的调用者读旧代不受影响。
        let store = TableStore::default();
        let gen1 = Arc::new(TableData {
            name: "t".into(),
            rows: Vec::new(),
        });
        store.insert(Arc::clone(&gen1));
        let holder = store.snapshot("t").expect("gen1");

        let gen2 = Arc::new(TableData {
            name: "t".into(),
            rows: Vec::new(),
        });
        store.insert(Arc::clone(&gen2));
        assert!(
            Arc::ptr_eq(&store.snapshot("t").unwrap(), &gen2),
            "store 已换代"
        );
        assert!(
            Arc::ptr_eq(&holder, &gen1),
            "旧 Arc 持有者仍指向完整的 gen1（不可变）"
        );
    }

    // -----------------------------------------------------------------------
    // RefreshService：tick → store 换代 + 信号（无数据载荷）
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn tick_swaps_store_and_signals_without_payload() {
        let mut service = RefreshService::spawn(vec![fixture_spec("address", "a")]);
        let signals = collect(&mut service, 2, Duration::from_millis(1500)).await;
        assert!(
            signals.len() >= 2,
            "期望 ≥2 次周期信号，实际 {}",
            signals.len()
        );
        for sig in &signals {
            assert_eq!(sig.name, "address");
        }
        // 数据不随信号走：调用者从 store pull 当前代（零复制）。
        let data = service
            .store
            .snapshot("address")
            .expect("换代的表应在 store 中有当前代");
        assert_eq!(data.rows.len(), 10, "address 表应重灌 10 行");
        let field = &data.rows[0][0];
        assert_eq!(field.get_name(), "value");
    }

    #[tokio::test]
    async fn multiple_specs_notify_concurrently_and_independently() {
        let mut service = RefreshService::spawn(vec![
            fixture_spec("address", "m1"),
            fixture_spec("example", "m2"),
        ]);
        let signals = collect(&mut service, 4, Duration::from_millis(2000)).await;
        let mut names: Vec<String> = signals.iter().map(|s| s.name.clone()).collect();
        names.sort();
        names.dedup();
        assert!(
            names.contains(&"address".to_string()) && names.contains(&"example".to_string()),
            "两表应各自独立换代出信号: {names:?}"
        );
        assert!(signals.len() >= 4, "期望 ≥4 次信号，实际 {}", signals.len());
        for name in ["address", "example"] {
            assert!(
                service.store.snapshot(name).is_some(),
                "{name} 换代后 store 应有当前代"
            );
        }
    }

    #[tokio::test]
    async fn zero_interval_spec_is_skipped() {
        let mut spec = fixture_spec("address", "z");
        spec.interval = Duration::ZERO;
        let mut service = RefreshService::spawn(vec![spec]);
        let signals = collect(&mut service, 1, Duration::from_millis(200)).await;
        assert!(signals.is_empty(), "零周期规格不应出信号");
    }

    #[tokio::test]
    async fn first_signal_not_before_first_interval() {
        // 首 tick（立即到期）被跳过：调用者已做启动装载；首信号应出现在
        // interval 之后而非启动瞬间。
        let mut spec = fixture_spec("address", "skip1");
        spec.interval = Duration::from_millis(250);
        let mut service = RefreshService::spawn(vec![spec]);

        let early = collect(&mut service, 1, Duration::from_millis(150)).await;
        assert!(early.is_empty(), "首 interval 前不应出信号");
        assert!(
            service.store.snapshot("address").is_none(),
            "首 tick 前 store 尚无当前代（等待调用者 seed / 首 interval 换代）"
        );

        let signals = collect(&mut service, 1, Duration::from_millis(800)).await;
        assert_eq!(signals.len(), 1, "首个 interval 后应恰好出 1 次信号");
        assert_eq!(signals[0].name, "address");
        assert_eq!(
            service.store.snapshot("address").expect("换代").rows.len(),
            10
        );
    }

    #[tokio::test]
    async fn reload_failure_is_skipped_and_shutdown_closes_channel() {
        // 未知表 → 每周期重载失败：只 warn 跳过（不 panic、服务存活）；
        // shutdown() 中止任务 → 信号通道关闭（recv 返回 None）。
        let mut spec = fixture_spec("ghost_table", "f");
        spec.interval = Duration::from_millis(60);
        let mut service = RefreshService::spawn(vec![spec]);

        let signals = collect(&mut service, 1, Duration::from_millis(400)).await;
        assert!(signals.is_empty(), "失败表不应出信号");
        assert!(
            service.store.snapshot("ghost_table").is_none(),
            "失败表不应换代"
        );

        service.shutdown();
        match tokio::time::timeout(Duration::from_millis(300), service.signals.recv()).await {
            Ok(None) => {}
            other => panic!("shutdown 后信号通道应关闭并 recv None，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn drop_aborts_tasks_and_closes_channel() {
        let mut service = RefreshService::spawn(vec![fixture_spec("address", "d")]);
        // 至少看到一次成功信号，再 drop 服务（中止任务 + 关通道）。
        let signals = collect(&mut service, 1, Duration::from_millis(1500)).await;
        assert_eq!(signals.len(), 1);
        drop(service);
        // drop 后无任务可再发信号：这里主要验证不 panic、干净退出（类型层面
        // receiver 已随 service 一并 drop）。
    }

    #[tokio::test(flavor = "current_thread")]
    async fn named_sql_spec_substitutes_code_before_each_query() {
        let _guard = crate::runtime::runtime_test_guard().lock_async().await;
        // 准备内存 provider（默认名）：两行 k=a/b，刷新查询按 $cur 过滤。
        let db = crate::mem::memdb::MemDB::instance();
        db.execute("CREATE TABLE refresh_vars_t (k TEXT, v TEXT)")
            .expect("create");
        db.execute("INSERT INTO refresh_vars_t VALUES ('a', '1'), ('b', '2')")
            .expect("seed");
        crate::facade::init_mem_provider(db).expect("init mem provider");
        let mut service = RefreshService::spawn(vec![RefreshSpec {
            name: "vars_t".into(),
            interval: Duration::from_millis(80),
            source: RefreshSource::NamedSql {
                provider: "default".to_string(),
                sql: "SELECT v FROM refresh_vars_t WHERE k = '$cur'".to_string(),
                code: "$cur = \"b\"".to_string(),
            },
        }]);
        let signals = collect(&mut service, 1, Duration::from_millis(1500)).await;
        assert_eq!(signals.len(), 1, "应出 1 次刷新信号");
        assert_eq!(signals[0].name, "vars_t");
        let data = service
            .store
            .snapshot("vars_t")
            .expect("刷新换代后应有当前代");
        assert_eq!(data.rows.len(), 1, "$cur→'b' 过滤后应只回 1 行");
        let field = &data.rows[0][0];
        assert_eq!(field.get_name(), "v");
        assert_eq!(field.to_string(), "chars(2)");
    }

    // -----------------------------------------------------------------------
    // load_rows（同步启动装载）：与 tick 同一渲染/查询语义
    // -----------------------------------------------------------------------

    #[test]
    fn sync_load_rows_substitutes_code_before_query() {
        let _guard = crate::runtime::runtime_test_guard().lock();
        // 独立表 + 内存 provider：验证同步装载路径的 VEL 替换与查询。
        let db = crate::mem::memdb::MemDB::instance();
        db.execute("CREATE TABLE sync_load_t (k TEXT, v TEXT)")
            .expect("create");
        db.execute("INSERT INTO sync_load_t VALUES ('a', '1'), ('b', '2')")
            .expect("seed");
        crate::facade::init_mem_provider(db).expect("init mem provider");
        let spec = RefreshSpec {
            name: "sync_t".into(),
            interval: Duration::from_millis(80),
            source: RefreshSource::NamedSql {
                provider: "default".to_string(),
                sql: "SELECT v FROM sync_load_t WHERE k = '$cur'".to_string(),
                code: "$cur = \"a\"".to_string(),
            },
        };
        let rows = load_rows(&spec).expect("同步装载应成功");
        assert_eq!(rows.len(), 1, "$cur→'a' 过滤后应只回 1 行");
        let field = &rows[0][0];
        assert_eq!(field.get_name(), "v");
    }

    #[test]
    fn sync_load_rows_authority_reloads_typed_rows() {
        // 同步装载的 Authority 臂（引擎 CSV 启动装载路径）：与 tick 内部同一
        // loader，重读 CSV → 重灌 → 类型化投影行。
        let spec = fixture_spec("address", "sync_auth");
        let rows = load_rows(&spec).expect("同步 Authority 装载应成功");
        assert_eq!(rows.len(), 10, "address 表应重灌 10 行");
        let field = &rows[0][0];
        assert_eq!(field.get_name(), "value");
    }

    // -----------------------------------------------------------------------
    // 共享 store（引擎接入语义）：seed → tick 换代同一库；失败/坏 code 保留旧代
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn spawn_with_store_replaces_seeded_generation_on_first_tick() {
        // 引擎 bootstrap 场景：调用者先 seed 一代（启动装载），daemon 用同一 store
        // 换代——首 tick 成功后 store 当前代 = 新代，旧 seed 代对持有者仍完整。
        let store = Arc::new(TableStore::default());
        let seed = Arc::new(TableData {
            name: "address".into(),
            rows: Vec::new(),
        });
        store.insert(Arc::clone(&seed));
        let mut service = RefreshService::spawn_with_store(
            vec![fixture_spec("address", "shared")],
            Arc::clone(&store),
        );

        let signals = collect(&mut service, 1, Duration::from_millis(1500)).await;
        assert_eq!(signals.len(), 1, "首 tick 后应出信号");
        let cur = store.snapshot("address").expect("tick 后应有当前代");
        assert_eq!(cur.rows.len(), 10, "fixture address 表 10 行");
        assert!(!Arc::ptr_eq(&cur, &seed), "tick 换代应替换启动 seed 代");
        assert!(seed.rows.is_empty(), "旧 seed 代不可变（持有者视角完整）");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_reload_keeps_last_generation_and_no_signal() {
        let _guard = crate::runtime::runtime_test_guard().lock_async().await;
        // provider 存在但查询指向不存在的表 → 每次 reload 失败：store 保留
        // 调用者 seed 的最后一代（不换代、不发信号、服务不退出）。
        let db = crate::mem::memdb::MemDB::instance();
        db.execute("CREATE TABLE refresh_fail_keep_t (k TEXT)")
            .expect("create");
        crate::facade::init_mem_provider(db).expect("init mem provider");

        let store = Arc::new(TableStore::default());
        let seed = Arc::new(TableData {
            name: "missing_t".into(),
            rows: Vec::new(),
        });
        store.insert(Arc::clone(&seed));
        let mut service = RefreshService::spawn_with_store(
            vec![RefreshSpec {
                name: "missing_t".into(),
                interval: Duration::from_millis(50),
                source: RefreshSource::NamedSql {
                    provider: "default".to_string(),
                    sql: "SELECT * FROM refresh_missing_xyz_t".to_string(),
                    code: String::new(),
                },
            }],
            Arc::clone(&store),
        );

        let signals = collect(&mut service, 1, Duration::from_millis(400)).await;
        assert!(signals.is_empty(), "失败表不应发信号");
        let cur = store.snapshot("missing_t").expect("seed 仍在");
        assert!(
            Arc::ptr_eq(&cur, &seed),
            "失败 tick 不应换代（保留最后一代）"
        );
    }

    #[tokio::test]
    async fn invalid_vel_code_skips_tick_and_load_rows_errors() {
        // VEL 渲染失败 = 配置错误：同步装载 fail-fast；tick 每次跳过（warn），
        // 不换代（store 无该表）、不发信号、任务存活。
        let spec = RefreshSpec {
            name: "velbad".into(),
            interval: Duration::from_millis(40),
            source: RefreshSource::NamedSql {
                provider: "default".to_string(),
                sql: "SELECT 1 WHERE '$bad'".to_string(),
                code: "$bad = nope(1)".to_string(),
            },
        };
        assert!(
            load_rows(&spec).is_err(),
            "坏 code 应在同步装载期（渲染）报错"
        );
        let mut service = RefreshService::spawn(vec![spec]);
        let signals = collect(&mut service, 1, Duration::from_millis(300)).await;
        assert!(signals.is_empty(), "渲染失败不应发信号");
        assert!(
            service.store.snapshot("velbad").is_none(),
            "渲染失败不应换代"
        );
    }

    #[tokio::test]
    async fn duplicate_spec_name_keeps_only_first_ticker() {
        // 同名重复登记（两个并行 ticker 换代同一表会让旧代覆盖新代）→
        // 只保留首个。确定性验证：首个间隔 1s（窗口内不会出信号）、重复的第二个
        // 间隔 200ms——若未去重，第二个任务会在 700ms 窗口内出 ≥1 次信号。
        let mut first_long = fixture_spec("address", "dup_long");
        first_long.interval = Duration::from_secs(1);
        let mut second_short = fixture_spec("address", "dup_short");
        second_short.interval = Duration::from_millis(200);
        let mut service = RefreshService::spawn(vec![first_long, second_short]);

        let signals = collect(&mut service, 3, Duration::from_millis(700)).await;
        assert!(
            signals.is_empty(),
            "重复同名 spec 应被去重：仅首个（1s）运行，短间隔第二个不得出信号"
        );
        assert!(
            service.store.snapshot("address").is_none(),
            "首 tick 未到不应换代"
        );

        // 反向顺序（短在前）：去重保留的是**首个**——短间隔任务运行 → 出信号。
        let mut first_short = fixture_spec("address", "dup_short2");
        first_short.interval = Duration::from_millis(200);
        let mut second_long = fixture_spec("address", "dup_long2");
        second_long.interval = Duration::from_secs(1);
        let mut service = RefreshService::spawn(vec![first_short, second_long]);
        let signals = collect(&mut service, 1, Duration::from_millis(1500)).await;
        assert_eq!(signals.len(), 1, "保留首个（短间隔）应出信号");
        assert_eq!(signals[0].name, "address");
        assert!(
            service.store.snapshot("address").is_some(),
            "短间隔任务应已换代"
        );
    }

    // -----------------------------------------------------------------------
    // TableStore 并发（写者换代 vs 读者 snapshot）——换代原子性
    // -----------------------------------------------------------------------

    #[test]
    fn store_concurrent_swap_and_snapshot_consistent() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;

        let store = Arc::new(TableStore::default());
        let gen_a = Arc::new(TableData {
            name: "t".into(),
            rows: Vec::new(),
        });
        let gen_b = Arc::new(TableData {
            name: "t".into(),
            rows: Vec::new(),
        });
        store.insert(Arc::clone(&gen_a));
        let stop = Arc::new(AtomicBool::new(false));

        let w_store = Arc::clone(&store);
        let w_a = Arc::clone(&gen_a);
        let w_b = Arc::clone(&gen_b);
        let w_stop = Arc::clone(&stop);
        let writer = thread::spawn(move || {
            for i in 0..5000u32 {
                w_store.insert(if i % 2 == 0 {
                    Arc::clone(&w_a)
                } else {
                    Arc::clone(&w_b)
                });
            }
            w_stop.store(true, Ordering::SeqCst);
        });

        let mut readers = Vec::new();
        for _ in 0..4 {
            let r_store = Arc::clone(&store);
            let r_a = Arc::clone(&gen_a);
            let r_b = Arc::clone(&gen_b);
            let r_stop = Arc::clone(&stop);
            readers.push(thread::spawn(move || {
                while !r_stop.load(Ordering::SeqCst) {
                    // 换代原子：snapshot 必为完整一代（A 或 B），绝不撕裂/未知。
                    let cur = r_store.snapshot("t").expect("首次 insert 后始终有当前代");
                    assert!(
                        Arc::ptr_eq(&cur, &r_a) || Arc::ptr_eq(&cur, &r_b),
                        "snapshot 应为完整一代（A/B 之一）"
                    );
                }
            }));
        }

        writer.join().expect("writer panicked");
        for r in readers {
            r.join().expect("reader panicked");
        }
        assert!(store.snapshot("t").is_some(), "结束后仍有当前代");
    }
}
