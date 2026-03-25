use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;

use orion_error::{ToStructError, UvsFrom};
use rusqlite::ToSql;
use rusqlite::{Connection, OpenFlags};
use wp_error::{KnowledgeReason, KnowledgeResult};
use wp_log::{info_ctrl, warn_kdb};
use wp_model_core::model::DataField;

use crate::DBQuery;
use crate::cache::CacheAble;
use crate::loader::{ProviderKind, parse_knowdb_conf};
use crate::mem::RowData;
use crate::mem::SqlNamedParam;
use crate::mem::memdb::MemDB;
use crate::mem::thread_clone::ThreadClonedMDB;
use crate::mysql::{MySqlProvider, MySqlProviderConfig};
use crate::param::named_params_to_fields;
use crate::postgres::{PostgresProvider, PostgresProviderConfig};

pub trait QueryFacade: Send + Sync {
    fn query(&self, sql: &str) -> KnowledgeResult<Vec<RowData>>;
    fn query_row(&self, sql: &str) -> KnowledgeResult<RowData>;
    fn query_named_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<RowData>;
}

fn query_named_fields_via_sqlite<Q: DBQuery>(
    provider: &Q,
    sql: &str,
    params: &[DataField],
) -> KnowledgeResult<RowData> {
    let named_params = params
        .iter()
        .cloned()
        .map(SqlNamedParam)
        .collect::<Vec<_>>();
    let refs: Vec<(&str, &dyn ToSql)> = named_params
        .iter()
        .map(|param| (param.0.get_name(), param as &dyn ToSql))
        .collect();
    DBQuery::query_row_params(provider, sql, refs.as_slice())
}

impl QueryFacade for ThreadClonedMDB {
    fn query(&self, sql: &str) -> KnowledgeResult<Vec<RowData>> {
        DBQuery::query(self, sql)
    }

    fn query_row(&self, sql: &str) -> KnowledgeResult<RowData> {
        DBQuery::query_row(self, sql)
    }

    fn query_named_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<RowData> {
        query_named_fields_via_sqlite(self, sql, params)
    }
}

struct MemProvider(MemDB);

impl QueryFacade for MemProvider {
    fn query(&self, sql: &str) -> KnowledgeResult<Vec<RowData>> {
        DBQuery::query(&self.0, sql)
    }

    fn query_row(&self, sql: &str) -> KnowledgeResult<RowData> {
        DBQuery::query_row(&self.0, sql)
    }

    fn query_named_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<RowData> {
        query_named_fields_via_sqlite(&self.0, sql, params)
    }
}

impl QueryFacade for PostgresProvider {
    fn query(&self, sql: &str) -> KnowledgeResult<Vec<RowData>> {
        PostgresProvider::query(self, sql)
    }

    fn query_row(&self, sql: &str) -> KnowledgeResult<RowData> {
        PostgresProvider::query_row(self, sql)
    }

    fn query_named_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<RowData> {
        PostgresProvider::query_named_fields(self, sql, params)
    }
}

impl QueryFacade for MySqlProvider {
    fn query(&self, sql: &str) -> KnowledgeResult<Vec<RowData>> {
        MySqlProvider::query(self, sql)
    }

    fn query_row(&self, sql: &str) -> KnowledgeResult<RowData> {
        MySqlProvider::query_row(self, sql)
    }

    fn query_named_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<RowData> {
        MySqlProvider::query_named_fields(self, sql, params)
    }
}

static PROVIDER: OnceLock<Arc<dyn QueryFacade>> = OnceLock::new();

pub fn init_thread_cloned_from_authority(authority_uri: &str) -> KnowledgeResult<()> {
    let tc = ThreadClonedMDB::from_authority(authority_uri);
    set_provider(Arc::new(tc))
}

pub fn init_mem_provider(memdb: MemDB) -> KnowledgeResult<()> {
    let res = set_provider(Arc::new(MemProvider(memdb)));
    if res.is_err() {
        eprintln!("[kdb] provider already initialized");
    } else {
        eprintln!("[kdb] provider set to MemProvider");
    }
    res
}

pub fn init_postgres_provider(connection_uri: &str, pool_size: Option<u32>) -> KnowledgeResult<()> {
    let config = PostgresProviderConfig::new(connection_uri).with_pool_size(pool_size);
    let provider = PostgresProvider::connect(&config)?;
    set_provider(Arc::new(provider))
}

pub fn init_mysql_provider(connection_uri: &str, pool_size: Option<u32>) -> KnowledgeResult<()> {
    let config = MySqlProviderConfig::new(connection_uri).with_pool_size(pool_size);
    let provider = MySqlProvider::connect(&config)?;
    set_provider(Arc::new(provider))
}

fn set_provider(p: Arc<dyn QueryFacade>) -> KnowledgeResult<()> {
    PROVIDER.set(p).map_err(|_| {
        KnowledgeReason::from_logic()
            .to_err()
            .with_detail("knowledge provider already initialized")
    })
}

fn get_provider() -> KnowledgeResult<&'static Arc<dyn QueryFacade>> {
    PROVIDER.get().ok_or_else(|| {
        KnowledgeReason::from_logic()
            .to_err()
            .with_detail("knowledge provider not initialized")
    })
}

pub fn query(sql: &str) -> KnowledgeResult<Vec<RowData>> {
    get_provider()?.query(sql)
}

pub fn query_row(sql: &str) -> KnowledgeResult<RowData> {
    get_provider()?.query_row(sql)
}

pub fn query_fields(sql: &str, params: &[DataField]) -> KnowledgeResult<RowData> {
    get_provider()?.query_named_fields(sql, params)
}

// Compatibility alias: prefer query_fields for new provider-neutral code.
pub fn query_named_fields(sql: &str, params: &[DataField]) -> KnowledgeResult<RowData> {
    query_fields(sql, params)
}

// Compatibility wrapper for existing SQLite-shaped call sites.
pub fn query_named<'a>(
    sql: &str,
    params: &'a [(&'a str, &'a dyn ToSql)],
) -> KnowledgeResult<RowData> {
    let fields = named_params_to_fields(params)?;
    query_fields(sql, &fields)
}

pub fn cache_query_fields<const N: usize>(
    sql: &str,
    c_params: &[DataField; N],
    query_params: &[DataField],
    cache: &mut impl CacheAble<DataField, RowData, N>,
) -> RowData {
    crate::cache_util::cache_query_impl(c_params, cache, || {
        if query_params.is_empty() {
            get_provider().and_then(|p| p.query_row(sql))
        } else {
            get_provider().and_then(|p| p.query_named_fields(sql, query_params))
        }
    })
}

// Compatibility wrapper for existing SQLite-shaped call sites.
pub fn cache_query<const N: usize>(
    sql: &str,
    c_params: &[DataField; N],
    named_params: &[(&str, &dyn ToSql)],
    cache: &mut impl CacheAble<DataField, RowData, N>,
) -> RowData {
    let query_fields = match named_params_to_fields(named_params) {
        Ok(fields) => fields,
        Err(err) => {
            warn_kdb!("[kdb] query param conversion error: {}", err);
            return Vec::new();
        }
    };

    cache_query_fields(sql, c_params, &query_fields, cache)
}

fn ensure_wal(authority_uri: &str) -> KnowledgeResult<()> {
    if let Ok(conn) = Connection::open_with_flags(
        authority_uri,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI,
    ) {
        let _ = conn.execute_batch(
            "PRAGMA journal_mode=WAL;\nPRAGMA synchronous=NORMAL;\nPRAGMA temp_store=MEMORY;",
        );
    }
    Ok(())
}

pub fn init_wal_pool_from_authority(authority_uri: &str, pool_size: u32) -> KnowledgeResult<()> {
    ensure_wal(authority_uri)?;
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let mem = MemDB::new_file(authority_uri, pool_size, flags)?;
    init_mem_provider(mem)
}

pub fn init_thread_cloned_from_knowdb(
    root: &Path,
    knowdb_conf: &Path,
    authority_uri: &str,
    dict: &orion_variate::EnvDict,
) -> KnowledgeResult<()> {
    let (conf, conf_abs, _) = parse_knowdb_conf(root, knowdb_conf, dict)?;
    if let Some(provider) = conf.provider {
        match provider.kind {
            ProviderKind::Postgres => {
                info_ctrl!("init postgres knowdb provider({}) ", conf_abs.display(),);
                return init_postgres_provider(
                    provider.connection_uri.as_str(),
                    provider.pool_size,
                );
            }
            ProviderKind::Mysql => {
                info_ctrl!("init mysql knowdb provider({}) ", conf_abs.display(),);
                return init_mysql_provider(provider.connection_uri.as_str(), provider.pool_size);
            }
            ProviderKind::SqliteAuthority => {}
        }
    }

    crate::loader::build_authority_from_knowdb(root, knowdb_conf, authority_uri, dict)?;
    let ro_uri = if let Some(rest) = authority_uri.strip_prefix("file:") {
        let path_part = rest.split('?').next().unwrap_or(rest);
        format!("file:{}?mode=ro&uri=true", path_part)
    } else {
        authority_uri.to_string()
    };
    let tc = ThreadClonedMDB::from_authority(&ro_uri);

    #[cfg(test)]
    {
        tc.with_tls_conn(|_| Ok(()))?;
    }

    info_ctrl!("init authority knowdb success({}) ", knowdb_conf.display(),);
    set_provider(Arc::new(tc))
}
