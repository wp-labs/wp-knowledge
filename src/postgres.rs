use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use orion_error::{ToStructError, UvsFrom};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::Mutex;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{Client, NoTls, Row, Statement};
use wp_error::{KnowledgeReason, KnowledgeResult};
use wp_log::warn_kdb;
use wp_model_core::model::{DataField, DataType, Value};

use crate::loader::ProviderKind;
use crate::mem::{RowData, query_util::metadata_cache_get_or_try_init_async_for_scope_typed};
use crate::runtime::MetadataCacheScope;

#[derive(Debug, Clone)]
pub struct PostgresProviderConfig {
    connection_uri: String,
    pool_size: u32,
}

impl PostgresProviderConfig {
    pub fn new(connection_uri: impl Into<String>) -> Self {
        Self {
            connection_uri: connection_uri.into(),
            pool_size: 8,
        }
    }

    pub fn connection_uri(&self) -> &str {
        &self.connection_uri
    }

    pub fn pool_size(&self) -> u32 {
        self.pool_size
    }

    pub fn with_pool_size(mut self, pool_size: Option<u32>) -> Self {
        if let Some(pool_size) = pool_size.filter(|size| *size > 0) {
            self.pool_size = pool_size;
        }
        self
    }
}

pub struct PostgresProvider {
    runtime: Option<Runtime>,
    connection_uri: String,
    clients: Vec<Arc<PostgresClientSlot>>,
    next_client: AtomicUsize,
    metadata_scope: MetadataCacheScope,
}

struct PostgresClientSlot {
    client: Mutex<Arc<Client>>,
}

impl PostgresClientSlot {
    fn new(client: Arc<Client>) -> Self {
        Self {
            client: Mutex::new(client),
        }
    }

    async fn active_client(&self, connection_uri: &str) -> KnowledgeResult<Arc<Client>> {
        let mut client = self.client.lock().await;
        if client.is_closed() {
            warn_kdb!("[kdb] postgres pooled client is closed; reconnecting");
            *client = Arc::new(connect_postgres_client(connection_uri).await?);
        }
        Ok(client.clone())
    }

    async fn reconnect_after_failure(
        &self,
        connection_uri: &str,
        failed_client: &Arc<Client>,
    ) -> KnowledgeResult<Arc<Client>> {
        let mut client = self.client.lock().await;
        if Arc::ptr_eq(&client, failed_client) || client.is_closed() {
            warn_kdb!("[kdb] postgres query hit a closed connection; reconnecting");
            *client = Arc::new(connect_postgres_client(connection_uri).await?);
        }
        Ok(client.clone())
    }
}

impl PostgresProvider {
    pub fn connect(
        config: &PostgresProviderConfig,
        metadata_scope: MetadataCacheScope,
    ) -> KnowledgeResult<Self> {
        let pool_size = config.pool_size().max(1) as usize;
        let connection_uri = config.connection_uri().to_string();
        let metadata_scope_for_thread = metadata_scope.clone();
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("wp-kdb-pg-init".to_string())
            .spawn(move || {
                let runtime = Builder::new_multi_thread()
                    .worker_threads(pool_size)
                    .enable_all()
                    .thread_name("wp-kdb-pg")
                    .build()
                    .map_err(|err| {
                        KnowledgeReason::from_conf()
                            .to_err()
                            .with_detail(format!("create postgres tokio runtime failed: {err}"))
                    });
                let result = runtime.and_then(|runtime| {
                    let connection_uri_for_clients = connection_uri.clone();
                    let clients = runtime.block_on(async move {
                        let mut clients = Vec::with_capacity(pool_size);
                        for _ in 0..pool_size {
                            let client =
                                connect_postgres_client(&connection_uri_for_clients).await?;
                            clients.push(Arc::new(PostgresClientSlot::new(Arc::new(client))));
                        }
                        Ok::<Vec<Arc<PostgresClientSlot>>, wp_error::KnowledgeError>(clients)
                    })?;
                    Ok::<PostgresProvider, wp_error::KnowledgeError>(Self {
                        runtime: Some(runtime),
                        connection_uri,
                        clients,
                        next_client: AtomicUsize::new(0),
                        metadata_scope: metadata_scope_for_thread,
                    })
                });
                let _ = tx.send(result);
            })
            .map_err(|err| {
                KnowledgeReason::from_conf()
                    .to_err()
                    .with_detail(format!("spawn postgres init thread failed: {err}"))
            })?;

        rx.recv()
            .map_err(|err| {
                KnowledgeReason::from_conf()
                    .to_err()
                    .with_detail(format!("receive postgres init result failed: {err}"))
            })
            .and_then(|result| result)
    }

    pub fn query(&self, sql: &str) -> KnowledgeResult<Vec<RowData>> {
        let slot = self.pick_client()?;
        let connection_uri = self.connection_uri.clone();
        let sql = sql.to_string();
        let metadata_scope = self.metadata_scope.clone();
        self.block_on_task(
            async move {
                execute_with_reconnect(slot, connection_uri, "query", move |client| {
                    let metadata_scope = metadata_scope.clone();
                    let sql = sql.clone();
                    async move { execute_query(&client, &metadata_scope, &sql).await }
                })
                .await
            },
            "query",
        )
    }

    pub fn query_row(&self, sql: &str) -> KnowledgeResult<RowData> {
        let slot = self.pick_client()?;
        let connection_uri = self.connection_uri.clone();
        let sql = sql.to_string();
        let metadata_scope = self.metadata_scope.clone();
        self.block_on_task(
            async move {
                execute_with_reconnect(slot, connection_uri, "query_row", move |client| {
                    let metadata_scope = metadata_scope.clone();
                    let sql = sql.clone();
                    async move { execute_query_row(&client, &metadata_scope, &sql).await }
                })
                .await
            },
            "query_row",
        )
    }

    pub fn query_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<Vec<RowData>> {
        let slot = self.pick_client()?;
        let connection_uri = self.connection_uri.clone();
        let sql = sql.to_string();
        let params = params.to_vec();
        let metadata_scope = self.metadata_scope.clone();
        self.block_on_task(
            async move {
                execute_with_reconnect(slot, connection_uri, "query_fields", move |client| {
                    let metadata_scope = metadata_scope.clone();
                    let sql = sql.clone();
                    let params = params.clone();
                    async move { execute_query_fields(&client, &metadata_scope, &sql, &params).await }
                })
                .await
            },
            "query_fields",
        )
    }

    pub fn query_named_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<RowData> {
        let slot = self.pick_client()?;
        let connection_uri = self.connection_uri.clone();
        let sql = sql.to_string();
        let params = params.to_vec();
        let metadata_scope = self.metadata_scope.clone();
        self.block_on_task(
            async move {
                execute_with_reconnect(slot, connection_uri, "query_named_fields", move |client| {
                    let metadata_scope = metadata_scope.clone();
                    let sql = sql.clone();
                    let params = params.clone();
                    async move {
                        execute_query_named_fields(&client, &metadata_scope, &sql, &params).await
                    }
                })
                .await
            },
            "query_named_fields",
        )
    }

    pub async fn query_async(&self, sql: &str) -> KnowledgeResult<Vec<RowData>> {
        let slot = self.pick_client()?;
        let connection_uri = self.connection_uri.clone();
        let sql = sql.to_string();
        let metadata_scope = self.metadata_scope.clone();
        self.run_task(
            async move {
                execute_with_reconnect(slot, connection_uri, "query", move |client| {
                    let metadata_scope = metadata_scope.clone();
                    let sql = sql.clone();
                    async move { execute_query(&client, &metadata_scope, &sql).await }
                })
                .await
            },
            "query",
        )
        .await
    }

    pub async fn query_row_async(&self, sql: &str) -> KnowledgeResult<RowData> {
        let slot = self.pick_client()?;
        let connection_uri = self.connection_uri.clone();
        let sql = sql.to_string();
        let metadata_scope = self.metadata_scope.clone();
        self.run_task(
            async move {
                execute_with_reconnect(slot, connection_uri, "query_row", move |client| {
                    let metadata_scope = metadata_scope.clone();
                    let sql = sql.clone();
                    async move { execute_query_row(&client, &metadata_scope, &sql).await }
                })
                .await
            },
            "query_row",
        )
        .await
    }

    pub async fn query_fields_async(
        &self,
        sql: &str,
        params: &[DataField],
    ) -> KnowledgeResult<Vec<RowData>> {
        let slot = self.pick_client()?;
        let connection_uri = self.connection_uri.clone();
        let sql = sql.to_string();
        let params = params.to_vec();
        let metadata_scope = self.metadata_scope.clone();
        self.run_task(
            async move {
                execute_with_reconnect(slot, connection_uri, "query_fields", move |client| {
                    let metadata_scope = metadata_scope.clone();
                    let sql = sql.clone();
                    let params = params.clone();
                    async move { execute_query_fields(&client, &metadata_scope, &sql, &params).await }
                })
                .await
            },
            "query_fields",
        )
        .await
    }

    pub async fn query_named_fields_async(
        &self,
        sql: &str,
        params: &[DataField],
    ) -> KnowledgeResult<RowData> {
        let slot = self.pick_client()?;
        let connection_uri = self.connection_uri.clone();
        let sql = sql.to_string();
        let params = params.to_vec();
        let metadata_scope = self.metadata_scope.clone();
        self.run_task(
            async move {
                execute_with_reconnect(slot, connection_uri, "query_named_fields", move |client| {
                    let metadata_scope = metadata_scope.clone();
                    let sql = sql.clone();
                    let params = params.clone();
                    async move {
                        execute_query_named_fields(&client, &metadata_scope, &sql, &params).await
                    }
                })
                .await
            },
            "query_named_fields",
        )
        .await
    }

    fn pick_client(&self) -> KnowledgeResult<Arc<PostgresClientSlot>> {
        if self.clients.is_empty() {
            return Err(KnowledgeReason::from_conf()
                .to_err()
                .with_detail("postgres client pool is empty"));
        }
        let idx = self.next_client.fetch_add(1, Ordering::Relaxed) % self.clients.len();
        Ok(self.clients[idx].clone())
    }

    async fn run_task<T, F>(&self, fut: F, action: &str) -> KnowledgeResult<T>
    where
        T: Send + 'static,
        F: Future<Output = KnowledgeResult<T>> + Send + 'static,
    {
        self.runtime
            .as_ref()
            .expect("postgres runtime available")
            .handle()
            .spawn(fut)
            .await
            .map_err(|err| join_err("postgres", action, err))?
    }

    fn block_on_task<T, F>(&self, fut: F, action: &str) -> KnowledgeResult<T>
    where
        T: Send + 'static,
        F: Future<Output = KnowledgeResult<T>> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        self.runtime
            .as_ref()
            .expect("postgres runtime available")
            .handle()
            .spawn(async move {
                let _ = tx.send(fut.await);
            });
        rx.recv().map_err(|err| {
            KnowledgeReason::from_logic().to_err().with_detail(format!(
                "postgres async task channel failed during {action}: {err}"
            ))
        })?
    }
}

impl Drop for PostgresProvider {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

async fn connect_postgres_client(connection_uri: &str) -> KnowledgeResult<Client> {
    let (client, connection) = tokio_postgres::connect(connection_uri, NoTls)
        .await
        .map_err(|err| {
            KnowledgeReason::from_conf()
                .to_err()
                .with_detail(format!("create postgres client failed: {err}"))
        })?;
    tokio::spawn(async move {
        if let Err(err) = connection.await {
            warn_kdb!("[kdb] postgres connection task ended: {}", err);
        }
    });
    client
        .simple_query("SELECT 1")
        .await
        .map_err(|err| validation_err("connection", err))?;
    Ok(client)
}

async fn execute_with_reconnect<T, F, Fut>(
    slot: Arc<PostgresClientSlot>,
    connection_uri: String,
    action: &str,
    execute: F,
) -> KnowledgeResult<T>
where
    F: Fn(Arc<Client>) -> Fut,
    Fut: Future<Output = Result<T, PostgresQueryError>>,
{
    let client = slot.active_client(&connection_uri).await?;
    match execute(client.clone()).await {
        Ok(value) => Ok(value),
        Err(PostgresQueryError::ConnectionClosed(err)) => {
            warn_kdb!(
                "[kdb] postgres {action} failed on a closed connection; retrying once: {}",
                err
            );
            let client = slot
                .reconnect_after_failure(&connection_uri, &client)
                .await?;
            execute(client)
                .await
                .map_err(PostgresQueryError::into_inner)
        }
        Err(err) if client.is_closed() => {
            warn_kdb!(
                "[kdb] postgres {action} closed the connection after failure; retrying once: {}",
                err.as_ref()
            );
            let client = slot
                .reconnect_after_failure(&connection_uri, &client)
                .await?;
            execute(client)
                .await
                .map_err(PostgresQueryError::into_inner)
        }
        Err(err) => Err(err.into_inner()),
    }
}

enum PostgresQueryError {
    ConnectionClosed(wp_error::KnowledgeError),
    Other(wp_error::KnowledgeError),
}

impl PostgresQueryError {
    fn as_ref(&self) -> &wp_error::KnowledgeError {
        match self {
            PostgresQueryError::ConnectionClosed(err) | PostgresQueryError::Other(err) => err,
        }
    }

    fn into_inner(self) -> wp_error::KnowledgeError {
        match self {
            PostgresQueryError::ConnectionClosed(err) | PostgresQueryError::Other(err) => err,
        }
    }
}

impl From<wp_error::KnowledgeError> for PostgresQueryError {
    fn from(err: wp_error::KnowledgeError) -> Self {
        Self::Other(err)
    }
}

fn pg_operation_err(stage: &str, err: tokio_postgres::Error) -> PostgresQueryError {
    let is_closed = err.is_closed();
    let err = KnowledgeReason::from_rule()
        .to_err()
        .with_detail(format!("postgres {stage} failed: {err}"));
    if is_closed {
        PostgresQueryError::ConnectionClosed(err)
    } else {
        PostgresQueryError::Other(err)
    }
}

async fn execute_query(
    client: &Client,
    metadata_scope: &MetadataCacheScope,
    sql: &str,
) -> Result<Vec<RowData>, PostgresQueryError> {
    let mut prepared_stmt = None;
    let col_names = metadata_cache_get_or_try_init_async_for_scope_typed(
        metadata_scope,
        Some(ProviderKind::Postgres),
        sql,
        || async {
            let stmt = client
                .prepare(sql)
                .await
                .map_err(|err| pg_operation_err("query prepare", err))?;
            let names = statement_col_names(&stmt);
            prepared_stmt = Some(stmt);
            Ok::<Option<Vec<String>>, PostgresQueryError>(Some(names))
        },
    )
    .await?;
    let rows = if let Some(stmt) = prepared_stmt.as_ref() {
        client.query(stmt, &[]).await
    } else {
        client.query(sql, &[]).await
    }
    .map_err(|err| pg_operation_err("query", err))?;
    rows.iter()
        .map(|row| map_row(row, &col_names).map_err(Into::into))
        .collect()
}

async fn execute_query_row(
    client: &Client,
    metadata_scope: &MetadataCacheScope,
    sql: &str,
) -> Result<RowData, PostgresQueryError> {
    let mut prepared_stmt = None;
    let col_names = metadata_cache_get_or_try_init_async_for_scope_typed(
        metadata_scope,
        Some(ProviderKind::Postgres),
        sql,
        || async {
            let stmt = client
                .prepare(sql)
                .await
                .map_err(|err| pg_operation_err("query_row prepare", err))?;
            let names = statement_col_names(&stmt);
            prepared_stmt = Some(stmt);
            Ok::<Option<Vec<String>>, PostgresQueryError>(Some(names))
        },
    )
    .await?;
    let row = if let Some(stmt) = prepared_stmt.as_ref() {
        client.query_opt(stmt, &[]).await
    } else {
        client.query_opt(sql, &[]).await
    }
    .map_err(|err| pg_operation_err("query_row", err))?;
    if let Some(row) = row.as_ref() {
        map_row(row, &col_names).map_err(Into::into)
    } else {
        Ok(Vec::new())
    }
}

async fn execute_query_fields(
    client: &Client,
    metadata_scope: &MetadataCacheScope,
    sql: &str,
    params: &[DataField],
) -> Result<Vec<RowData>, PostgresQueryError> {
    let (rewritten_sql, ordered_params) = rewrite_sql(sql, params)?;
    let bind_values: Vec<PostgresBindValue> = ordered_params
        .iter()
        .map(|field| PostgresBindValue::from(*field))
        .collect();
    let bind_refs: Vec<&(dyn ToSql + Sync)> = bind_values
        .iter()
        .map(PostgresBindValue::as_tosql)
        .collect();

    let mut prepared_stmt = None;
    let col_names = metadata_cache_get_or_try_init_async_for_scope_typed(
        metadata_scope,
        Some(ProviderKind::Postgres),
        sql,
        || async {
            let stmt = client
                .prepare(&rewritten_sql)
                .await
                .map_err(|err| pg_operation_err("query_fields prepare", err))?;
            let names = statement_col_names(&stmt);
            prepared_stmt = Some(stmt);
            Ok::<Option<Vec<String>>, PostgresQueryError>(Some(names))
        },
    )
    .await?;
    let rows = if let Some(stmt) = prepared_stmt.as_ref() {
        client.query(stmt, &bind_refs).await
    } else {
        client.query(&rewritten_sql, &bind_refs).await
    }
    .map_err(|err| pg_operation_err("query_fields", err))?;
    rows.iter()
        .map(|row| map_row(row, &col_names).map_err(Into::into))
        .collect()
}

async fn execute_query_named_fields(
    client: &Client,
    metadata_scope: &MetadataCacheScope,
    sql: &str,
    params: &[DataField],
) -> Result<RowData, PostgresQueryError> {
    let (rewritten_sql, ordered_params) = rewrite_sql(sql, params)?;
    let bind_values: Vec<PostgresBindValue> = ordered_params
        .iter()
        .map(|field| PostgresBindValue::from(*field))
        .collect();
    let bind_refs: Vec<&(dyn ToSql + Sync)> = bind_values
        .iter()
        .map(PostgresBindValue::as_tosql)
        .collect();

    let mut prepared_stmt = None;
    let col_names = metadata_cache_get_or_try_init_async_for_scope_typed(
        metadata_scope,
        Some(ProviderKind::Postgres),
        sql,
        || async {
            let stmt = client
                .prepare(&rewritten_sql)
                .await
                .map_err(|err| pg_operation_err("query_named_fields prepare", err))?;
            let names = statement_col_names(&stmt);
            prepared_stmt = Some(stmt);
            Ok::<Option<Vec<String>>, PostgresQueryError>(Some(names))
        },
    )
    .await?;
    let row = if let Some(stmt) = prepared_stmt.as_ref() {
        client.query_opt(stmt, &bind_refs).await
    } else {
        client.query_opt(&rewritten_sql, &bind_refs).await
    }
    .map_err(|err| pg_operation_err("query_named_fields", err))?;
    if let Some(row) = row.as_ref() {
        map_row(row, &col_names).map_err(Into::into)
    } else {
        Ok(Vec::new())
    }
}

#[derive(Debug)]
enum PostgresBindValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Null(Option<String>),
}

impl PostgresBindValue {
    fn as_tosql(&self) -> &(dyn ToSql + Sync) {
        match self {
            PostgresBindValue::Bool(value) => value,
            PostgresBindValue::Int(value) => value,
            PostgresBindValue::Float(value) => value,
            PostgresBindValue::Text(value) => value,
            PostgresBindValue::Null(value) => value,
        }
    }
}

impl From<&DataField> for PostgresBindValue {
    fn from(field: &DataField) -> Self {
        match field.get_value() {
            Value::Bool(value) => PostgresBindValue::Bool(*value),
            Value::Digit(value) => PostgresBindValue::Int(*value),
            Value::Float(value) => PostgresBindValue::Float(*value),
            Value::Null | Value::Ignore(_) => PostgresBindValue::Null(None),
            Value::Chars(value) => PostgresBindValue::Text(value.to_string()),
            Value::Symbol(value) => PostgresBindValue::Text(value.to_string()),
            Value::Time(value) => PostgresBindValue::Text(value.to_string()),
            Value::Hex(value) => PostgresBindValue::Text(value.to_string()),
            Value::IpNet(value) => PostgresBindValue::Text(value.to_string()),
            Value::IpAddr(value) => PostgresBindValue::Text(value.to_string()),
            Value::Obj(value) => PostgresBindValue::Text(format!("{:?}", value)),
            Value::Array(value) => PostgresBindValue::Text(format!("{:?}", value)),
            Value::Domain(value) => PostgresBindValue::Text(value.0.to_string()),
            Value::Url(value) => PostgresBindValue::Text(value.0.to_string()),
            Value::Email(value) => PostgresBindValue::Text(value.0.to_string()),
            Value::IdCard(value) => PostgresBindValue::Text(value.0.to_string()),
            Value::MobilePhone(value) => PostgresBindValue::Text(value.0.to_string()),
        }
    }
}

fn validation_err(stage: &str, err: tokio_postgres::Error) -> wp_error::KnowledgeError {
    KnowledgeReason::from_conf().to_err().with_detail(format!(
        "postgres startup validation failed during {stage}: connection issue: {err}"
    ))
}

fn join_err(provider: &str, action: &str, err: tokio::task::JoinError) -> wp_error::KnowledgeError {
    KnowledgeReason::from_logic().to_err().with_detail(format!(
        "{provider} async task join failed during {action}: {err}"
    ))
}

fn normalize_param_name(name: &str) -> String {
    if name.starts_with(':') {
        name.to_string()
    } else {
        format!(":{}", name)
    }
}

fn rewrite_sql<'a>(
    sql: &str,
    params: &'a [DataField],
) -> KnowledgeResult<(String, Vec<&'a DataField>)> {
    let mut by_name = HashMap::with_capacity(params.len());
    for field in params {
        by_name.insert(normalize_param_name(field.get_name()), field);
    }

    let mut assigned_numbers: HashMap<String, usize> = HashMap::new();
    let mut ordered: Vec<&DataField> = Vec::new();
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0usize;
    let mut dollar_tag: Option<Vec<u8>> = None;

    while i < bytes.len() {
        if let Some(tag) = dollar_tag.as_ref() {
            if bytes[i..].starts_with(tag) {
                out.push_str(std::str::from_utf8(tag).expect("valid dollar quote tag"));
                i += tag.len();
                dollar_tag = None;
                continue;
            }
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }

        match bytes[i] {
            b'\'' => {
                out.push('\'');
                i += 1;
                while i < bytes.len() {
                    out.push(bytes[i] as char);
                    if bytes[i] == b'\'' {
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'\'' {
                            out.push('\'');
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'"' => {
                out.push('"');
                i += 1;
                while i < bytes.len() {
                    out.push(bytes[i] as char);
                    if bytes[i] == b'"' {
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'"' {
                            out.push('"');
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                out.push('-');
                out.push('-');
                i += 2;
                while i < bytes.len() {
                    out.push(bytes[i] as char);
                    let is_newline = bytes[i] == b'\n';
                    i += 1;
                    if is_newline {
                        break;
                    }
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                out.push('/');
                out.push('*');
                i += 2;
                while i < bytes.len() {
                    out.push(bytes[i] as char);
                    if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                        out.push('/');
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            b'$' => {
                if let Some(tag_len) = parse_dollar_quote_tag(&bytes[i..]) {
                    let tag = bytes[i..i + tag_len].to_vec();
                    out.push_str(std::str::from_utf8(&tag).expect("valid dollar quote tag"));
                    i += tag_len;
                    dollar_tag = Some(tag);
                } else {
                    out.push('$');
                    i += 1;
                }
            }
            b':' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b':' {
                    out.push(':');
                    out.push(':');
                    i += 2;
                    continue;
                }
                if i + 1 >= bytes.len() || !is_param_start(bytes[i + 1]) {
                    out.push(':');
                    i += 1;
                    continue;
                }

                let start = i;
                i += 2;
                while i < bytes.len() && is_param_continue(bytes[i]) {
                    i += 1;
                }
                let raw_name = &sql[start..i];
                let field = by_name.get(raw_name).ok_or_else(|| {
                    KnowledgeReason::from_rule()
                        .to_err()
                        .with_detail(format!("postgres query missing param: {raw_name}"))
                })?;
                let placeholder_no = if let Some(idx) = assigned_numbers.get(raw_name) {
                    *idx
                } else {
                    let idx = ordered.len() + 1;
                    assigned_numbers.insert(raw_name.to_string(), idx);
                    ordered.push(*field);
                    idx
                };
                out.push('$');
                out.push_str(&placeholder_no.to_string());
            }
            _ => {
                out.push(bytes[i] as char);
                i += 1;
            }
        }
    }

    Ok((out, ordered))
}

fn parse_dollar_quote_tag(input: &[u8]) -> Option<usize> {
    if input.first().copied()? != b'$' {
        return None;
    }
    let mut idx = 1usize;
    while idx < input.len() && input[idx] != b'$' {
        if idx == 1 {
            if !is_dollar_tag_start(input[idx]) {
                return None;
            }
        } else if !is_dollar_tag_continue(input[idx]) {
            return None;
        }
        idx += 1;
    }
    if idx >= input.len() || input[idx] != b'$' {
        return None;
    }
    Some(idx + 1)
}

fn is_param_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_param_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_dollar_tag_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_dollar_tag_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn map_row(row: &Row, col_names: &[String]) -> KnowledgeResult<RowData> {
    let mut out = Vec::with_capacity(row.len());
    for (idx, col) in row.columns().iter().enumerate() {
        let col_name = col_names
            .get(idx)
            .map(|name| name.as_str())
            .unwrap_or(col.name());
        out.push(map_value(row, idx, col_name, col.type_())?);
    }
    Ok(out)
}

fn statement_col_names(stmt: &Statement) -> Vec<String> {
    stmt.columns()
        .iter()
        .map(|col| col.name().to_string())
        .collect()
}

fn map_value(row: &Row, idx: usize, name: &str, ty: &Type) -> KnowledgeResult<DataField> {
    match *ty {
        Type::BOOL => Ok(row
            .try_get::<usize, Option<bool>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_bool(name.to_string(), value))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))),
        Type::INT2 => Ok(row
            .try_get::<usize, Option<i16>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_digit(name.to_string(), i64::from(value)))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))),
        Type::INT4 => Ok(row
            .try_get::<usize, Option<i32>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_digit(name.to_string(), i64::from(value)))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))),
        Type::INT8 | Type::OID => Ok(row
            .try_get::<usize, Option<i64>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_digit(name.to_string(), value))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))),
        Type::FLOAT4 => Ok(row
            .try_get::<usize, Option<f32>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_float(name.to_string(), f64::from(value)))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))),
        Type::FLOAT8 => Ok(row
            .try_get::<usize, Option<f64>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_float(name.to_string(), value))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))),
        Type::VARCHAR | Type::TEXT | Type::BPCHAR | Type::NAME | Type::UNKNOWN => Ok(row
            .try_get::<usize, Option<String>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_chars(name.to_string(), value))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))),
        Type::TIMESTAMP => Ok(row
            .try_get::<usize, Option<NaiveDateTime>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_chars(name.to_string(), value.to_string()))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))),
        Type::TIMESTAMPTZ => Ok(row
            .try_get::<usize, Option<DateTime<Utc>>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_chars(name.to_string(), value.to_rfc3339()))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))),
        Type::DATE => Ok(row
            .try_get::<usize, Option<NaiveDate>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_chars(name.to_string(), value.to_string()))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))),
        Type::TIME => Ok(row
            .try_get::<usize, Option<NaiveTime>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_chars(name.to_string(), value.to_string()))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))),
        Type::JSON | Type::JSONB => Ok(row
            .try_get::<usize, Option<serde_json::Value>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_chars(name.to_string(), value.to_string()))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))),
        _ => Ok(row
            .try_get::<usize, Option<String>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_chars(name.to_string(), value))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))),
    }
}

fn pg_decode_err(err: tokio_postgres::Error) -> wp_error::KnowledgeError {
    KnowledgeReason::from_rule()
        .to_err()
        .with_detail(format!("postgres row decode failed: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wp_model_core::model::DataField;

    #[test]
    fn rewrite_sql_skips_pg_cast_comments_and_dollar_quotes() {
        let sql = r#"
SELECT
  payload::jsonb,
  note
FROM demo
WHERE id = :id
  AND note <> ':ignored'
  AND tag = $$:ignored$$
  -- :ignored
  /* :ignored */
"#;
        let params = [DataField::from_digit(":id", 7)];
        let (rewritten, ordered) = rewrite_sql(sql, &params).expect("rewrite sql");
        assert!(rewritten.contains("payload::jsonb"));
        assert!(rewritten.contains("id = $1"));
        assert!(rewritten.contains("':ignored'"));
        assert!(rewritten.contains("$$:ignored$$"));
        assert_eq!(ordered.len(), 1);
        assert_eq!(ordered[0].get_name(), ":id");
    }

    #[test]
    fn rewrite_sql_reuses_same_placeholder_for_duplicate_param() {
        let sql = "SELECT * FROM demo WHERE left_id=:id OR right_id=:id";
        let params = [DataField::from_digit(":id", 7)];
        let (rewritten, ordered) = rewrite_sql(sql, &params).expect("rewrite sql");
        assert_eq!(
            rewritten,
            "SELECT * FROM demo WHERE left_id=$1 OR right_id=$1"
        );
        assert_eq!(ordered.len(), 1);
    }
}
