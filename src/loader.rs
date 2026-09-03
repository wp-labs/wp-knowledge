use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use orion_conf::EnvTomlLoad;
use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use wp_log::info_ctrl;

use crate::error::{KnowReason, KnowledgeResult};
use crate::mem::memdb::MemDB;
use orion_error::OperationContext;
use orion_error::conversion::{SourceErr, SourceRawErr, ToStructError};
use orion_variate::EnvDict;
use rusqlite::OpenFlags;

/// V2 KnowDB 配置：目录式 + 外置 SQL。仅支持单一数据文件：`<table_dir>/data.csv`，
/// 或通过 `tables[n].data_file` 相对 `<table_dir>` 指定。
#[derive(Debug, Deserialize)]
pub struct KnowDbConf {
    pub version: u32,
    #[serde(default = "default_dot")]
    pub base_dir: String,
    #[serde(default)]
    pub default: OptLoadSpec,
    #[serde(default)]
    pub csv: CsvSpec,
    #[serde(default)]
    pub cache: CacheSpec,
    #[serde(default)]
    pub tables: Vec<TableSpec>,

    /// `[fun.<name>]` — external named-query definitions.
    #[serde(default)]
    pub fun: HashMap<String, FunSpec>,

    /// Raw provider config — `[provider.sqldb]` / `[provider.redis]`.
    #[serde(default, rename = "provider")]
    provider_raw: Option<ProviderConfig>,

    /// `[intranet_nets]` — 内网网段知识配置
    #[serde(default)]
    pub intranet_nets: Option<crate::intranet_nets::IntranetNetsConf>,
}

impl KnowDbConf {
    pub fn provider(&self) -> Option<ProviderConfig> {
        self.provider_raw.clone()
    }
}

// ---------------------------------------------------------------------------
// Fun (external named query) config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct FunSpec {
    pub call: FunCall,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default = "default_true")]
    pub cache: bool,
    #[serde(default)]
    pub ttl_ms: Option<u64>,
}

impl FunSpec {
    /// Derive return type from the call (bf_exists/sismember → bool, hget/get → value).
    pub fn returns_bool(&self) -> bool {
        matches!(self.call, FunCall::BfExists | FunCall::Sismember)
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FunCall {
    BfExists,
    Sismember,
    Hget,
    Get,
}

// ---------------------------------------------------------------------------
// Cache config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct CacheSpec {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_result_cache_capacity")]
    pub capacity: usize,
    #[serde(default = "default_result_cache_ttl_ms")]
    pub ttl_ms: u64,
}

impl Default for CacheSpec {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            capacity: default_result_cache_capacity(),
            ttl_ms: default_result_cache_ttl_ms(),
        }
    }
}

// ---------------------------------------------------------------------------
// Provider configuration (new format: [provider.sqldb] / [provider.redis])
// ---------------------------------------------------------------------------

/// SQL 数据库 provider 的默认生效名：未显式 `name` 时使用。
pub const DEFAULT_SQLDB_NAME: &str = "default";

/// PostgreSQL `plan_cache_mode` 枚举值（`[provider.sqldb.postgres_session]`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanCacheMode {
    #[default]
    Auto,
    ForceGenericPlan,
    ForceCustomPlan,
}

impl PlanCacheMode {
    /// PostgreSQL `SET plan_cache_mode` 接受的取值。
    pub fn to_sql_value(self) -> &'static str {
        match self {
            PlanCacheMode::Auto => "auto",
            PlanCacheMode::ForceGenericPlan => "force_generic_plan",
            PlanCacheMode::ForceCustomPlan => "force_custom_plan",
        }
    }
}

/// PostgreSQL 连接级 session 初始化配置（稳定执行计划用）。
///
/// 对应 `[provider.sqldb.postgres_session]`；仅 `kind = "postgres"` 生效，
/// 其他 kind 配置该字段会在 [`SqlProviderSpec::validate_specs`] 被拒绝。
/// 未配置（`None`）时保持默认行为；每一项 `None` 表示该项不下发。
/// 不提供任意 after_connect_sql 入口。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostgresSessionSpec {
    #[serde(default)]
    pub plan_cache_mode: Option<PlanCacheMode>,
    /// `None` = 不下发 `SET jit`（保持数据库默认）。
    #[serde(default)]
    pub jit: Option<bool>,
    /// `None` = 不下发 `SET application_name`。长度 ≤ 63 字节（PG `NAMEDATALEN-1`），无控制字符。
    #[serde(default)]
    pub application_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProviderConfig {
    /// 支持 `[provider.sqldb]`（单个）或 `[[provider.sqldb]]`（多个）两种写法。
    #[serde(default, deserialize_with = "deserialize_sqldb")]
    pub sqldb: Option<Vec<SqlProviderSpec>>,
    #[serde(default)]
    pub redis: Option<RedisProviderSpec>,
}

impl ProviderConfig {
    /// 返回全部 sqldb provider 的生效名列表（无显式 `name` 时为 `default`）。
    pub fn sqldb_names(&self) -> Vec<String> {
        self.sqldb
            .as_ref()
            .map(|specs| {
                specs
                    .iter()
                    .map(|spec| spec.effective_name().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SqlProviderSpec {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "kind")]
    pub kind: SqlProviderKind,
    pub connection_uri: String,
    #[serde(default)]
    pub pool_size: Option<u32>,
    #[serde(default)]
    pub min_connections: Option<u32>,
    #[serde(default)]
    pub acquire_timeout_ms: Option<u64>,
    #[serde(default)]
    pub idle_timeout_ms: Option<u64>,
    #[serde(default)]
    pub max_lifetime_ms: Option<u64>,
    /// PostgreSQL 专属：连接级 session 初始化（稳定执行计划）；仅 `kind = "postgres"` 消费。
    #[serde(default)]
    pub postgres_session: Option<PostgresSessionSpec>,
}

impl SqlProviderSpec {
    /// 生效名称：显式 `name` 缺失时回退到 [`DEFAULT_SQLDB_NAME`]。
    pub fn effective_name(&self) -> &str {
        self.name.as_deref().unwrap_or(DEFAULT_SQLDB_NAME)
    }

    /// 校验 provider 名称字符集（仅 `[A-Za-z0-9_]`）与重名，返回生效名称集合。
    pub fn validate_specs(specs: &[SqlProviderSpec]) -> KnowledgeResult<HashSet<String>> {
        let mut seen = HashSet::with_capacity(specs.len());
        for spec in specs {
            let name = spec.effective_name();
            if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(KnowReason::from_conf().to_err().with_detail(format!(
                    "invalid sqldb provider name '{name}' (allowed [A-Za-z0-9_])"
                )));
            }
            if !seen.insert(name.to_string()) {
                return Err(KnowReason::from_conf()
                    .to_err()
                    .with_detail(format!("duplicate sqldb provider name '{name}'")));
            }
            if let Some(session) = &spec.postgres_session {
                if !matches!(spec.kind, SqlProviderKind::Postgres) {
                    return Err(KnowReason::from_conf().to_err().with_detail(format!(
                        "provider '{name}': postgres_session is only valid for kind = \"postgres\""
                    )));
                }
                if let Some(app) = &session.application_name {
                    if app.len() > 63 || app.chars().any(|c| c.is_control()) {
                        return Err(KnowReason::from_conf().to_err().with_detail(format!(
                            "provider '{name}': postgres_session.application_name invalid \
                             (≤ 63 bytes, no control characters)"
                        )));
                    }
                }
            }
        }
        Ok(seen)
    }
}

/// 同时接受 `[provider.sqldb]`（单表）与 `[[provider.sqldb]]`（数组）两种 TOML 写法。
fn deserialize_sqldb<'de, D>(deserializer: D) -> Result<Option<Vec<SqlProviderSpec>>, D::Error>
where
    D: Deserializer<'de>,
{
    struct SqlDbVisitor;

    impl<'de> Visitor<'de> for SqlDbVisitor {
        type Value = Option<Vec<SqlProviderSpec>>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter
                .write_str("a `[provider.sqldb]` table or a `[[provider.sqldb]]` array of tables")
        }

        fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let spec = SqlProviderSpec::deserialize(de::value::MapAccessDeserializer::new(map))?;
            Ok(Some(vec![spec]))
        }

        fn visit_seq<A>(self, seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let specs =
                Vec::<SqlProviderSpec>::deserialize(de::value::SeqAccessDeserializer::new(seq))?;
            Ok(Some(specs))
        }
    }

    deserializer.deserialize_any(SqlDbVisitor)
}

/// Config-level SQL provider kind — Postgres and Mysql only.
/// The runtime-level [`ProviderKind`] additionally includes `SqliteAuthority`
/// and `Redis` for the internal provider registry.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlProviderKind {
    Postgres,
    Mysql,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RedisProviderSpec {
    pub connection_uri: String,
    #[serde(default)]
    pub pool_size: Option<usize>,
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default = "default_command_timeout_ms")]
    pub command_timeout_ms: u64,
}

fn default_connect_timeout_ms() -> u64 {
    3_000
}

fn default_command_timeout_ms() -> u64 {
    100
}

/// Runtime-level provider kind — used by the internal registry to identify
/// the active provider. Includes built-in types (SqliteAuthority) and all
/// external types (Postgres, Mysql, Redis).
///
/// For config-level SQL providers, see [`SqlProviderKind`].
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    SqliteAuthority,
    Postgres,
    Mysql,
    Redis,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OptLoadSpec {
    #[serde(default = "default_true")]
    pub transaction: bool,
    #[serde(default = "default_batch")]
    pub batch_size: usize,
    #[serde(default = "default_on_error")]
    pub on_error: OnError,
}
impl Default for OptLoadSpec {
    fn default() -> Self {
        Self {
            transaction: true,
            batch_size: default_batch(),
            on_error: default_on_error(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OnError {
    #[default]
    Fail,
    Skip,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CsvSpec {
    #[serde(default = "default_true")]
    pub has_header: bool,
    #[serde(default = "default_comma")]
    pub delimiter: String,
    #[serde(default = "default_utf8")]
    pub encoding: String,
    #[serde(default = "default_true")]
    pub trim: bool,
}
impl Default for CsvSpec {
    fn default() -> Self {
        CsvSpec {
            has_header: true,
            delimiter: ",".into(),
            encoding: "utf-8".into(),
            trim: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TableSpec {
    pub name: String,
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(default)]
    pub data_file: Option<String>,
    pub columns: ColumnsSpec,
    #[serde(default)]
    pub expected_rows: RowExpect,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ColumnsSpec {
    #[serde(default)]
    pub by_header: Vec<String>,
    #[serde(default)]
    pub by_index: Vec<usize>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RowExpect {
    pub min: Option<usize>,
    pub max: Option<usize>,
}

const fn default_true() -> bool {
    true
}
const fn default_batch() -> usize {
    2000
}
fn default_comma() -> String {
    ",".to_string()
}
fn default_utf8() -> String {
    "utf-8".to_string()
}
fn default_on_error() -> OnError {
    OnError::Fail
}
fn default_dot() -> String {
    ".".to_string()
}
const fn default_result_cache_capacity() -> usize {
    1024
}
const fn default_result_cache_ttl_ms() -> u64 {
    30_000
}

/// 读取文本文件，返回字符串
fn read_to_string(path: &Path) -> KnowledgeResult<String> {
    let mut f = fs::File::open(path).source_raw_err(KnowReason::from_res(), "source error")?;
    let mut buf = String::new();
    f.read_to_string(&mut buf)
        .source_raw_err(KnowReason::from_res(), "source error")?;
    Ok(buf)
}

fn replace_table(sql: &str, table: &str) -> String {
    sql.replace("{table}", table)
}

fn join_rel(base: &Path, rel: &str) -> PathBuf {
    let p = Path::new(rel);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

pub fn build_authority_from_knowdb(
    root: &Path,
    conf_path: &Path,
    authority_uri: &str,
    dict: &EnvDict,
) -> KnowledgeResult<Vec<String>> {
    let mut opx = OperationContext::doing("build authority from knowdb").with_auto_log();
    // 1) 解析配置与 base_dir
    let (conf, conf_abs, base_dir) = parse_knowdb_conf(root, conf_path, dict)?;
    opx.record("conf", conf_abs.display());
    opx.record("base_dir", base_dir.display());
    // 2) 打开权威库
    let db = open_authority(authority_uri)?;
    // 3) 逐表加载（按配置顺序）；不再处理显式依赖
    let mut loaded_names = Vec::new();
    for t in &conf.tables {
        if !t.enabled {
            continue;
        }
        load_one_table(&db, &base_dir, t, &conf.csv, &conf.default)?;
        info_ctrl!("load table {} suc!", base_dir.display(),);
        loaded_names.push(t.name.clone());
    }
    opx.mark_suc();
    Ok(loaded_names)
}

pub fn parse_knowdb_conf(
    root: &Path,
    conf_path: &Path,
    dict: &EnvDict,
) -> KnowledgeResult<(KnowDbConf, PathBuf, PathBuf)> {
    let conf_abs = if conf_path.is_absolute() {
        conf_path.to_path_buf()
    } else {
        root.join(conf_path)
    };
    let conf_txt = read_to_string(&conf_abs)?;
    let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(&conf_txt, dict)
        .source_err(KnowReason::from_conf(), "parse knowdb config")?;
    if conf.version != 2 {
        return Err(KnowReason::from_conf()
            .to_err()
            .with_detail("unsupported knowdb.version"));
    }
    // 注入内网网段配置（`[intranet_nets]` 节），供规则引擎消费
    crate::intranet_nets::set_intranet_nets_conf(conf.intranet_nets.clone());
    let conf_dir = conf_abs.parent().unwrap_or_else(|| Path::new("."));
    let base_dir = join_rel(conf_dir, &conf.base_dir);
    Ok((conf, conf_abs, base_dir))
}

fn open_authority(authority_uri: &str) -> KnowledgeResult<MemDB> {
    ensure_parent_dir_for_file_uri(authority_uri);
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_URI;
    let db = MemDB::new_file(authority_uri, 1, flags)?;
    // 预注册内置 UDF 至权威库连接（注意：连接池可能返回不同连接，导入时也会再次注册）
    let _ = db.with_conn(|conn| {
        let _ = crate::sqlite_ext::register_builtin(conn);
        Ok::<(), anyhow::Error>(())
    });
    Ok(db)
}

/// Kahn 拓扑排序：返回按依赖顺序的表索引列表。
/// no topo_sort_tables: V2 简化版按配置顺序加载
fn ensure_parent_dir_for_file_uri(uri: &str) {
    if let Some(rest) = uri.strip_prefix("file:") {
        let path_part = rest.split('?').next().unwrap_or(rest);
        let p = Path::new(path_part);
        if let Some(parent) = p.parent() {
            let _ = fs::create_dir_all(parent);
        }
    }
}

fn load_one_table(
    db: &MemDB,
    base_dir: &Path,
    t: &TableSpec,
    csvd: &CsvSpec,
    load: &OptLoadSpec,
) -> KnowledgeResult<()> {
    // 目录与必须文件
    let mut opx = OperationContext::doing("load table to kdb")
        .with_auto_log()
        .with_mod_path("ctrl");
    let dir_name: &str = t.dir.as_deref().unwrap_or(&t.name);
    let table_dir = base_dir.join(dir_name);
    opx.record("table_dir", table_dir.display());
    let create_sql = replace_table(&read_to_string(&table_dir.join("create.sql"))?, &t.name);
    let insert_sql = replace_table(&read_to_string(&table_dir.join("insert.sql"))?, &t.name);
    let clean_path = table_dir.join("clean.sql");
    let clean_sql = if clean_path.exists() {
        replace_table(&read_to_string(&clean_path)?, &t.name)
    } else {
        format!("DELETE FROM {}", t.name)
    };

    // 建表与清理
    db.with_conn(|conn| {
        // 注册内置 UDF（导入连接）
        let _ = crate::sqlite_ext::register_builtin(conn);
        conn.execute_batch(&create_sql)?;
        conn.execute_batch(&clean_sql)?;
        Ok::<(), anyhow::Error>(())
    })
    .source_err(KnowReason::from_res(), "prepare authority table")?;

    // 数据源
    let data_path = match &t.data_file {
        Some(rel) => join_rel(&table_dir, rel),
        None => table_dir.join("data.csv"),
    };
    if !data_path.exists() {
        return Err(KnowReason::from_conf()
            .to_err()
            .with_detail("data.csv not found"));
    }
    opx.record("data_path", data_path.display());

    // CSV 解析器
    let mut rdr = build_csv_reader(csvd, &data_path)?;

    // 列映射
    let col_indices: Vec<usize> = if !t.columns.by_header.is_empty() {
        let headers = rdr
            .headers()
            .source_raw_err(KnowReason::from_res(), "source error")?;
        select_indices_by_header(headers, &t.columns.by_header)?
    } else if !t.columns.by_index.is_empty() {
        t.columns.by_index.clone()
    } else {
        return Err(KnowReason::from_conf()
            .to_err()
            .with_detail("columns mapping required"));
    };

    // 导入（分批事务）
    let mut inserted: usize = 0;
    let mut bad: usize = 0;
    let mut batch_left = load.batch_size.max(1);
    db.with_conn(|conn| {
        // 注册内置 UDF（用于 INSERT 绑定表达式）
        let _ = crate::sqlite_ext::register_builtin(conn);
        let mut tx = if load.transaction {
            Some(conn.unchecked_transaction()?)
        } else {
            None
        };
        let mut stmt = conn.prepare(&insert_sql)?;
        for rec in rdr.into_records() {
            match rec {
                Ok(record) => {
                    let refs = extract_row_refs(&record, &col_indices, &mut bad, load)?;
                    if let Some(refs) = refs {
                        stmt.execute(rusqlite::params_from_iter(refs))?;
                        inserted += 1;
                        if load.transaction {
                            batch_left -= 1;
                            if batch_left == 0 {
                                tx.take().unwrap().commit()?;
                                tx = Some(conn.unchecked_transaction()?);
                                batch_left = load.batch_size.max(1);
                            }
                        }
                    }
                }
                Err(_e) => {
                    if matches!(load.on_error, OnError::Skip) {
                        bad += 1;
                        continue;
                    } else {
                        anyhow::bail!("csv record parse error");
                    }
                }
            }
        }
        if let Some(tx) = tx {
            tx.commit()?;
        }
        Ok::<(), anyhow::Error>(())
    })
    .source_err(KnowReason::from_res(), "load authority table data")?;

    // 行数校验
    if let Some(min) = t.expected_rows.min
        && inserted < min
    {
        return Err(KnowReason::from_conf()
            .to_err()
            .with_detail("table data less"));
    }
    if let Some(max) = t.expected_rows.max
        && inserted > max
    {
        wp_log::warn_kdb!(
            "table {} loaded rows {} exceed max {}",
            &t.name,
            inserted,
            max
        );
    }
    if bad > 0 {
        wp_log::warn_kdb!("table {} skipped {} bad rows (on_error=skip)", &t.name, bad);
    }
    opx.mark_suc();
    Ok(())
}

fn build_csv_reader(
    csvd: &CsvSpec,
    data_path: &Path,
) -> KnowledgeResult<csv::Reader<std::fs::File>> {
    if csvd.encoding.to_lowercase() != "utf-8" {
        return Err(KnowReason::from_conf()
            .to_err()
            .with_detail("only utf-8 csv is supported"));
    }
    let mut rdr_b = csv::ReaderBuilder::new();
    rdr_b.has_headers(csvd.has_header);
    if csvd.delimiter.len() == 1 {
        rdr_b.delimiter(csvd.delimiter.as_bytes()[0]);
    }
    if csvd.trim {
        rdr_b.trim(csv::Trim::All);
    }
    rdr_b
        .from_path(data_path)
        .source_raw_err(KnowReason::from_res(), "source error")
}

fn select_indices_by_header(
    headers: &csv::StringRecord,
    wanted: &[String],
) -> KnowledgeResult<Vec<usize>> {
    let mut out = Vec::with_capacity(wanted.len());
    for name in wanted {
        let pos = headers.iter().position(|h| h == name).ok_or_else(|| {
            KnowReason::from_conf()
                .to_err()
                .with_detail("header not found")
        })?;
        out.push(pos);
    }
    Ok(out)
}

fn extract_row_refs<'a>(
    record: &'a csv::StringRecord,
    col_indices: &[usize],
    bad: &mut usize,
    load: &OptLoadSpec,
) -> anyhow::Result<Option<Vec<&'a str>>> {
    let mut vs: Vec<&str> = Vec::with_capacity(col_indices.len());
    for &idx in col_indices {
        if idx >= record.len() {
            if matches!(load.on_error, OnError::Skip) {
                *bad += 1;
                return Ok(None);
            } else {
                anyhow::bail!("missing column at index {}", idx);
            }
        }
        vs.push(record.get(idx).unwrap_or(""));
    }
    Ok(Some(vs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_new_style_sqldb_provider() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2

[provider.sqldb]
kind = "postgres"
connection_uri = "postgres://demo:demo@127.0.0.1/demo"
pool_size = 12
"#,
            &dict,
        )
        .expect("parse knowdb with sqldb provider");

        let sqldb = conf
            .provider()
            .expect("provider")
            .sqldb
            .expect("sqldb provider");
        assert_eq!(sqldb.len(), 1);
        let sqldb = &sqldb[0];
        assert!(matches!(sqldb.kind, SqlProviderKind::Postgres));
        assert_eq!(sqldb.pool_size, Some(12));
        assert_eq!(sqldb.effective_name(), DEFAULT_SQLDB_NAME);
    }

    #[test]
    fn parse_new_style_redis_provider() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2

[provider.redis]
connection_uri = "redis://127.0.0.1:6379"
pool_size = 16
connect_timeout_ms = 5000
command_timeout_ms = 200
"#,
            &dict,
        )
        .expect("parse knowdb with redis provider");

        let redis_cfg = conf
            .provider()
            .expect("provider")
            .redis
            .expect("redis provider");
        assert_eq!(redis_cfg.connection_uri, "redis://127.0.0.1:6379");
        assert_eq!(redis_cfg.pool_size, Some(16));
        assert_eq!(redis_cfg.connect_timeout_ms, 5000);
        assert_eq!(redis_cfg.command_timeout_ms, 200);
    }

    #[test]
    fn parse_redis_provider_with_default_timeouts() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2

[provider.redis]
connection_uri = "redis://127.0.0.1:6379"
"#,
            &dict,
        )
        .expect("parse knowdb with redis provider (no timeout fields)");

        let redis_cfg = conf.provider().expect("provider").redis.expect("redis");
        assert_eq!(redis_cfg.connect_timeout_ms, 3000);
        assert_eq!(redis_cfg.command_timeout_ms, 100);
    }

    #[test]
    fn parse_both_sqldb_and_redis_providers() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2

[provider.sqldb]
kind = "postgres"
connection_uri = "postgres://demo:demo@127.0.0.1/demo"

[provider.redis]
connection_uri = "redis://10.0.0.1:6379"
pool_size = 4
"#,
            &dict,
        )
        .expect("parse knowdb with both sqldb and redis");

        let provider_cfg = conf.provider().expect("provider");
        let sqldb = &provider_cfg.sqldb.expect("sqldb")[0];
        let redis_cfg = provider_cfg.redis.expect("redis");
        assert!(matches!(sqldb.kind, SqlProviderKind::Postgres));
        assert_eq!(redis_cfg.connection_uri, "redis://10.0.0.1:6379");
        assert_eq!(redis_cfg.pool_size, Some(4));
    }

    #[test]
    fn parse_redis_only_without_sqldb() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2

[provider.redis]
connection_uri = "redis://127.0.0.1:6379"
"#,
            &dict,
        )
        .expect("parse knowdb with redis only");

        let provider_cfg = conf.provider().expect("provider");
        assert!(provider_cfg.sqldb.is_none());
        assert!(provider_cfg.redis.is_some());
    }

    #[test]
    fn parse_no_provider_section() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2
"#,
            &dict,
        )
        .expect("parse knowdb without provider");

        assert!(conf.provider().is_none());
    }

    #[test]
    fn new_style_sqldb_mysql_variant() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2

[provider.sqldb]
kind = "mysql"
connection_uri = "mysql://user:pass@127.0.0.1:3306/db"
pool_size = 8
"#,
            &dict,
        )
        .expect("parse new-style mysql sqldb");

        let sqldb = &conf.provider().expect("provider").sqldb.expect("sqldb")[0];
        assert!(matches!(sqldb.kind, SqlProviderKind::Mysql));
        assert_eq!(sqldb.pool_size, Some(8));
    }

    #[test]
    fn parse_array_style_multi_sqldb_providers() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2

[[provider.sqldb]]
name = "geo"
kind = "postgres"
connection_uri = "postgres://demo@127.0.0.1:5432/geo_db"
pool_size = 8

[[provider.sqldb]]
name = "asset"
kind = "postgres"
connection_uri = "postgres://demo@127.0.0.1:5432/asset_db"
pool_size = 12
"#,
            &dict,
        )
        .expect("parse knowdb with multiple sqldb providers");

        let specs = conf
            .provider()
            .expect("provider")
            .sqldb
            .expect("sqldb providers");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name.as_deref(), Some("geo"));
        assert_eq!(specs[0].effective_name(), "geo");
        assert_eq!(
            specs[0].connection_uri,
            "postgres://demo@127.0.0.1:5432/geo_db"
        );
        assert_eq!(specs[0].pool_size, Some(8));
        assert_eq!(specs[1].name.as_deref(), Some("asset"));
        assert_eq!(specs[1].effective_name(), "asset");
        assert_eq!(
            specs[1].connection_uri,
            "postgres://demo@127.0.0.1:5432/asset_db"
        );
        assert_eq!(specs[1].pool_size, Some(12));
    }

    #[test]
    fn parse_single_sqldb_with_explicit_name() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2

[provider.sqldb]
name = "geo"
kind = "postgres"
connection_uri = "postgres://demo@127.0.0.1/geo_db"
"#,
            &dict,
        )
        .expect("parse single named sqldb");

        let spec = &conf.provider().expect("provider").sqldb.expect("sqldb")[0];
        assert_eq!(spec.effective_name(), "geo");
    }

    #[test]
    fn sqldb_names_applies_default_name() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2

[provider.sqldb]
kind = "postgres"
connection_uri = "postgres://demo@127.0.0.1/demo"
"#,
            &dict,
        )
        .expect("parse unnamed sqldb");

        let names = conf.provider().expect("provider").sqldb_names();
        assert_eq!(names, vec!["default".to_string()]);
    }

    #[test]
    fn validate_specs_rejects_duplicate_effective_names() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2

[[provider.sqldb]]
kind = "postgres"
connection_uri = "postgres://demo@127.0.0.1/db1"

[[provider.sqldb]]
kind = "postgres"
connection_uri = "postgres://demo@127.0.0.1/db2"
"#,
            &dict,
        )
        .expect("parse two unnamed sqldb");

        let specs = conf.provider().expect("provider").sqldb.expect("sqldb");
        let err = SqlProviderSpec::validate_specs(&specs).expect_err("duplicate 'default' name");
        assert!(
            err.to_string()
                .contains("duplicate sqldb provider name 'default'")
        );
    }

    #[test]
    fn validate_specs_rejects_invalid_name_charset() {
        let specs = [SqlProviderSpec {
            name: Some("bad name".to_string()),
            kind: SqlProviderKind::Postgres,
            connection_uri: "postgres://demo@127.0.0.1/db".to_string(),
            pool_size: None,
            min_connections: None,
            acquire_timeout_ms: None,
            idle_timeout_ms: None,
            max_lifetime_ms: None,
            postgres_session: None,
        }];
        let err = SqlProviderSpec::validate_specs(&specs).expect_err("invalid name charset");
        assert!(err.to_string().contains("invalid sqldb provider name"));
    }

    #[test]
    fn parse_sqldb_postgres_session() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2

[provider.sqldb]
name = "geo"
kind = "postgres"
connection_uri = "postgres://demo@127.0.0.1/geo_db"

[provider.sqldb.postgres_session]
plan_cache_mode = "force_generic_plan"
jit = false
application_name = "ip_geo_service"
"#,
            &dict,
        )
        .expect("parse postgres_session config");

        let spec = &conf.provider().expect("provider").sqldb.expect("sqldb")[0];
        let session = spec.postgres_session.as_ref().expect("postgres_session");
        assert_eq!(
            session.plan_cache_mode,
            Some(PlanCacheMode::ForceGenericPlan)
        );
        assert_eq!(session.jit, Some(false));
        assert_eq!(session.application_name.as_deref(), Some("ip_geo_service"));
    }

    #[test]
    fn parse_sqldb_postgres_session_unknown_key_rejected() {
        let dict = EnvDict::default();
        let r = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2

[provider.sqldb]
kind = "postgres"
connection_uri = "postgres://demo@127.0.0.1/demo"

[provider.sqldb.postgres_session]
plan_cache_mode = "auto"
after_connect_sql = "SET x = 1"
"#,
            &dict,
        );
        assert!(
            r.is_err(),
            "unknown postgres_session key (after_connect_sql) should be rejected"
        );
    }

    #[test]
    fn validate_specs_rejects_session_on_non_postgres() {
        let specs = [SqlProviderSpec {
            name: Some("geo".to_string()),
            kind: SqlProviderKind::Mysql,
            connection_uri: "mysql://demo@127.0.0.1/db".to_string(),
            pool_size: None,
            min_connections: None,
            acquire_timeout_ms: None,
            idle_timeout_ms: None,
            max_lifetime_ms: None,
            postgres_session: Some(PostgresSessionSpec {
                plan_cache_mode: Some(PlanCacheMode::ForceGenericPlan),
                jit: None,
                application_name: None,
            }),
        }];
        let err = SqlProviderSpec::validate_specs(&specs).expect_err("mysql with postgres_session");
        assert!(err.to_string().contains("postgres_session"));
    }

    #[test]
    fn validate_specs_rejects_application_name_too_long() {
        let specs = [SqlProviderSpec {
            name: Some("geo".to_string()),
            kind: SqlProviderKind::Postgres,
            connection_uri: "postgres://demo@127.0.0.1/db".to_string(),
            pool_size: None,
            min_connections: None,
            acquire_timeout_ms: None,
            idle_timeout_ms: None,
            max_lifetime_ms: None,
            postgres_session: Some(PostgresSessionSpec {
                plan_cache_mode: None,
                jit: None,
                application_name: Some("x".repeat(64)),
            }),
        }];
        let err = SqlProviderSpec::validate_specs(&specs).expect_err("application_name too long");
        assert!(err.to_string().contains("application_name"));
    }

    #[test]
    fn parse_empty_sqldb_array_is_empty() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2

[provider]
sqldb = []
"#,
            &dict,
        )
        .expect("parse empty sqldb array");

        let specs = conf
            .provider()
            .expect("provider")
            .sqldb
            .expect("sqldb present");
        assert!(specs.is_empty());
    }

    #[test]
    fn mixed_named_and_unnamed_sqldb_applies_default_name() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2

[[provider.sqldb]]
name = "geo"
kind = "postgres"
connection_uri = "postgres://demo@127.0.0.1/geo_db"

[[provider.sqldb]]
kind = "postgres"
connection_uri = "postgres://demo@127.0.0.1/asset_db"
"#,
            &dict,
        )
        .expect("parse mixed named/unnamed sqldb");

        let specs = conf.provider().expect("provider").sqldb.expect("sqldb");
        assert_eq!(specs.len(), 2);
        // 有显式 name 的生效名是 name；无 name 的生效名是 default
        assert_eq!(specs[0].effective_name(), "geo");
        assert_eq!(specs[1].effective_name(), DEFAULT_SQLDB_NAME);

        // validate_specs 对混合配置应通过，且默认库规则为 "default" 优先
        let expected = SqlProviderSpec::validate_specs(&specs).expect("valid specs");
        assert!(expected.contains(DEFAULT_SQLDB_NAME));
    }

    #[test]
    fn explicit_default_name_is_recognized() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2

[[provider.sqldb]]
name = "default"
kind = "postgres"
connection_uri = "postgres://demo@127.0.0.1/main_db"

[[provider.sqldb]]
name = "geo"
kind = "postgres"
connection_uri = "postgres://demo@127.0.0.1/geo_db"
"#,
            &dict,
        )
        .expect("parse explicit default name");

        let specs = conf.provider().expect("provider").sqldb.expect("sqldb");
        assert_eq!(specs[0].effective_name(), DEFAULT_SQLDB_NAME);
        assert_eq!(specs[1].effective_name(), "geo");
        let expected = SqlProviderSpec::validate_specs(&specs).expect("valid specs");
        assert!(expected.contains(DEFAULT_SQLDB_NAME));
    }

    #[test]
    fn parse_cache_spec_with_defaults() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2
"#,
            &dict,
        )
        .expect("parse knowdb with default cache spec");

        assert!(conf.cache.enabled);
        assert_eq!(conf.cache.capacity, 1024);
        assert_eq!(conf.cache.ttl_ms, 30_000);
    }

    #[test]
    fn parse_cache_spec_from_toml() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2

[cache]
enabled = false
capacity = 256
ttl_ms = 1500
"#,
            &dict,
        )
        .expect("parse knowdb with cache spec");

        assert!(!conf.cache.enabled);
        assert_eq!(conf.cache.capacity, 256);
        assert_eq!(conf.cache.ttl_ms, 1500);
    }

    #[test]
    fn parse_redis_cache_spec() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2

[cache]
enabled = true
capacity = 512
"#,
            &dict,
        )
        .expect("parse knowdb with cache");

        assert!(conf.cache.enabled);
        assert_eq!(conf.cache.capacity, 512);
    }

    #[test]
    fn parse_redis_cache_defaults() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2
"#,
            &dict,
        )
        .expect("parse knowdb without redis.cache");

        // No [cache] → defaults enabled=true, capacity=1024
        assert!(conf.cache.enabled);
        assert_eq!(conf.cache.capacity, 1024);
    }

    // -----------------------------------------------------------------------
    // Fun (external named query) config tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_fun_bool_services() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2

[fun.password_check]
call = "bf_exists"
key = "weak_passwords"

[fun.ip_whitelist]
call = "sismember"
key = "allowed_ips"
"#,
            &dict,
        )
        .expect("parse fun bool services");

        let pw = conf.fun.get("password_check").expect("password_check");
        assert_eq!(pw.call, FunCall::BfExists);
        assert_eq!(pw.key.as_deref(), Some("weak_passwords"));
        assert!(pw.returns_bool());

        let ip = conf.fun.get("ip_whitelist").expect("ip_whitelist");
        assert_eq!(ip.call, FunCall::Sismember);
        assert_eq!(ip.key.as_deref(), Some("allowed_ips"));
        assert!(ip.returns_bool());
    }

    #[test]
    fn parse_fun_value_services() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2

[fun.threat_actor]
call = "hget"
key = "threat_actors"
cache = true
ttl_ms = 60000

[fun.user_tag]
call = "get"
"#,
            &dict,
        )
        .expect("parse fun value services");

        let ta = conf.fun.get("threat_actor").expect("threat_actor");
        assert_eq!(ta.call, FunCall::Hget);
        assert_eq!(ta.key.as_deref(), Some("threat_actors"));
        assert!(ta.cache);
        assert_eq!(ta.ttl_ms, Some(60000));
        assert!(!ta.returns_bool());

        let ut = conf.fun.get("user_tag").expect("user_tag");
        assert_eq!(ut.call, FunCall::Get);
        assert!(ut.key.is_none());
        assert!(ut.cache); // default true
        assert!(!ut.returns_bool());
    }

    #[test]
    fn parse_fun_default_cache() {
        let dict = EnvDict::default();
        let conf: KnowDbConf = <KnowDbConf as EnvTomlLoad<KnowDbConf>>::env_parse_toml(
            r#"
version = 2

[fun.app_config]
call = "get"
key = "app_config"
"#,
            &dict,
        )
        .expect("parse fun default cache");

        let spec = conf.fun.get("app_config").expect("app_config");
        assert!(spec.cache);
        assert!(spec.ttl_ms.is_none());
    }
}
