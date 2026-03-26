use std::cell::RefCell;
use std::time::Duration;

use crate::DBQuery;
use crate::mem::RowData;
use orion_error::{ErrorOwe, ErrorWith};
use rusqlite::ToSql;
use rusqlite::backup::Backup;
use rusqlite::{Connection, Params};
use wp_error::KnowledgeResult;
use wp_log::debug_kdb;
use wp_model_core::model::DataField;

use super::SqlNamedParam;

thread_local! {
    // clippy: use const init for thread_local value
    static TLS_DB: RefCell<Option<ThreadLocalState>> = const { RefCell::new(None) };
}

struct ThreadLocalState {
    authority_path: String,
    generation: u64,
    conn: Connection,
}

/// Thread-cloned read-only in-memory DB built from an authority file DB via SQLite backup API.
/// Each thread lazily creates its own in-memory Connection (no cross-thread sharing).
#[derive(Clone)]
pub struct ThreadClonedMDB {
    authority_path: String,
    generation: u64,
}

impl ThreadClonedMDB {
    pub fn from_authority(path: &str) -> Self {
        Self {
            authority_path: path.to_string(),
            generation: 0,
        }
    }

    pub fn from_authority_with_generation(path: &str, generation: u64) -> Self {
        Self {
            authority_path: path.to_string(),
            generation,
        }
    }

    pub fn with_tls_conn<T, F: FnOnce(&Connection) -> KnowledgeResult<T>>(
        &self,
        f: F,
    ) -> KnowledgeResult<T> {
        let path = self.authority_path.clone();
        let generation = self.generation;
        TLS_DB.with(|cell| {
            // make sure a thread-local in-memory db exists
            let should_rebuild = cell
                .borrow()
                .as_ref()
                .map(|state| state.authority_path != path || state.generation != generation)
                .unwrap_or(true);
            if should_rebuild {
                debug_kdb!(
                    "[kdb] rebuild thread-local sqlite snapshot generation={} path={}",
                    generation,
                    path
                );
                // source: authority file; dest: in-memory
                let src = Connection::open_with_flags(
                    &path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                        | rusqlite::OpenFlags::SQLITE_OPEN_URI,
                )
                .owe_res()
                .want("connect db")?;
                let mut dst = Connection::open_in_memory().owe_res().want("oepn conn")?;
                {
                    let bk = Backup::new(&src, &mut dst).owe_conf().want("backup")?;
                    // Copy all pages with small sleep to yield
                    bk.run_to_completion(50, Duration::from_millis(0), None)
                        .owe_res()
                        .want("backup run")?;
                }
                // 为查询连接注册内置 UDF（只读场景也可用在 SQL/OML 查询中）
                let _ = crate::sqlite_ext::register_builtin(&dst);
                *cell.borrow_mut() = Some(ThreadLocalState {
                    authority_path: path.clone(),
                    generation,
                    conn: dst,
                });
            }
            // safe to unwrap since ensured above
            let conn = cell.borrow();
            f(&conn.as_ref().unwrap().conn)
        })
    }

    pub fn query_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<Vec<RowData>> {
        self.with_tls_conn(|conn| {
            let named_params = params
                .iter()
                .cloned()
                .map(SqlNamedParam)
                .collect::<Vec<_>>();
            let refs: Vec<(&str, &dyn ToSql)> = named_params
                .iter()
                .map(|param| (param.0.get_name(), param as &dyn ToSql))
                .collect();
            super::query_util::query_cached(conn, sql, refs.as_slice())
        })
    }

    pub fn query_named_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<RowData> {
        self.query_fields(sql, params)
            .map(|rows| rows.into_iter().next().unwrap_or_default())
    }
}

impl DBQuery for ThreadClonedMDB {
    fn query(&self, sql: &str) -> KnowledgeResult<Vec<RowData>> {
        self.with_tls_conn(|conn| super::query_util::query(conn, sql, []))
    }
    fn query_row(&self, sql: &str) -> KnowledgeResult<RowData> {
        self.with_tls_conn(|conn| super::query_util::query_first_row(conn, sql, []))
    }

    fn query_row_params<P: Params>(&self, sql: &str, params: P) -> KnowledgeResult<RowData> {
        self.with_tls_conn(|conn| super::query_util::query_first_row(conn, sql, params))
    }

    fn query_row_tdos<P: Params>(
        &self,
        _sql: &str,
        _params: &[DataField; 2],
    ) -> KnowledgeResult<RowData> {
        // not used in current benchmarks
        Ok(vec![])
    }
}
