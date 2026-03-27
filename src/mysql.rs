use std::future::Future;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use ::mysql_async::prelude::Queryable;
use ::mysql_async::{
    Opts, OptsBuilder, Params, Pool, PoolConstraints, PoolOpts, Row, Statement, Value as MySqlValue,
};
use orion_error::{ToStructError, UvsFrom};
use tokio::runtime::{Builder, Runtime};
use wp_error::{KnowledgeReason, KnowledgeResult};
use wp_model_core::model::{DataField, DataType, Value};

use crate::loader::ProviderKind;
use crate::mem::{RowData, query_util::metadata_cache_get_or_try_init_async_for_scope};
use crate::runtime::MetadataCacheScope;

#[derive(Debug, Clone)]
pub struct MySqlProviderConfig {
    connection_uri: String,
    pool_size: u32,
}

impl MySqlProviderConfig {
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

pub struct MySqlProvider {
    runtime: Option<Runtime>,
    pool: Pool,
    metadata_scope: MetadataCacheScope,
}

impl MySqlProvider {
    pub fn connect(
        config: &MySqlProviderConfig,
        metadata_scope: MetadataCacheScope,
    ) -> KnowledgeResult<Self> {
        let pool_size = config.pool_size().max(1) as usize;
        let opts = Opts::from_url(config.connection_uri()).map_err(|err| {
            KnowledgeReason::from_conf()
                .to_err()
                .with_detail(format!("parse mysql connection uri failed: {err}"))
        })?;
        let constraints = PoolConstraints::new(0, pool_size).ok_or_else(|| {
            KnowledgeReason::from_conf()
                .to_err()
                .with_detail("invalid mysql pool constraints")
        })?;
        let pool_opts = PoolOpts::default()
            .with_constraints(constraints)
            .with_inactive_connection_ttl(Duration::from_secs(60));
        let opts = OptsBuilder::from_opts(opts).pool_opts(pool_opts);
        let pool = Pool::new(Opts::from(opts));
        let metadata_scope_for_thread = metadata_scope.clone();
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("wp-kdb-mysql-init".to_string())
            .spawn(move || {
                let runtime = Builder::new_multi_thread()
                    .worker_threads(pool_size)
                    .enable_all()
                    .thread_name("wp-kdb-mysql")
                    .build()
                    .map_err(|err| {
                        KnowledgeReason::from_conf()
                            .to_err()
                            .with_detail(format!("create mysql tokio runtime failed: {err}"))
                    });
                let result = runtime.and_then(|runtime| {
                    runtime.block_on(validate_startup(&pool))?;
                    Ok::<MySqlProvider, wp_error::KnowledgeError>(Self {
                        runtime: Some(runtime),
                        pool,
                        metadata_scope: metadata_scope_for_thread,
                    })
                });
                let _ = tx.send(result);
            })
            .map_err(|err| {
                KnowledgeReason::from_conf()
                    .to_err()
                    .with_detail(format!("spawn mysql init thread failed: {err}"))
            })?;
        rx.recv().map_err(|err| {
            KnowledgeReason::from_conf()
                .to_err()
                .with_detail(format!("receive mysql init result failed: {err}"))
        })?
    }

    pub fn query(&self, sql: &str) -> KnowledgeResult<Vec<RowData>> {
        let pool = self.pool.clone();
        let sql = sql.to_string();
        let metadata_scope = self.metadata_scope.clone();
        self.block_on_task(
            async move { execute_query(pool, metadata_scope, sql).await },
            "query",
        )
    }

    pub fn query_row(&self, sql: &str) -> KnowledgeResult<RowData> {
        let pool = self.pool.clone();
        let sql = sql.to_string();
        let metadata_scope = self.metadata_scope.clone();
        self.block_on_task(
            async move { execute_query_row(pool, metadata_scope, sql).await },
            "query_row",
        )
    }

    pub fn query_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<Vec<RowData>> {
        let pool = self.pool.clone();
        let sql = sql.to_string();
        let params = params.to_vec();
        let metadata_scope = self.metadata_scope.clone();
        self.block_on_task(
            async move { execute_query_fields(pool, metadata_scope, sql, params).await },
            "query_fields",
        )
    }

    pub fn query_named_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<RowData> {
        let pool = self.pool.clone();
        let sql = sql.to_string();
        let params = params.to_vec();
        let metadata_scope = self.metadata_scope.clone();
        self.block_on_task(
            async move { execute_query_named_fields(pool, metadata_scope, sql, params).await },
            "query_named_fields",
        )
    }

    pub async fn query_async(&self, sql: &str) -> KnowledgeResult<Vec<RowData>> {
        let pool = self.pool.clone();
        let sql = sql.to_string();
        let metadata_scope = self.metadata_scope.clone();
        self.run_task(
            async move { execute_query(pool, metadata_scope, sql).await },
            "query",
        )
        .await
    }

    pub async fn query_row_async(&self, sql: &str) -> KnowledgeResult<RowData> {
        let pool = self.pool.clone();
        let sql = sql.to_string();
        let metadata_scope = self.metadata_scope.clone();
        self.run_task(
            async move { execute_query_row(pool, metadata_scope, sql).await },
            "query_row",
        )
        .await
    }

    pub async fn query_fields_async(
        &self,
        sql: &str,
        params: &[DataField],
    ) -> KnowledgeResult<Vec<RowData>> {
        let pool = self.pool.clone();
        let sql = sql.to_string();
        let params = params.to_vec();
        let metadata_scope = self.metadata_scope.clone();
        self.run_task(
            async move { execute_query_fields(pool, metadata_scope, sql, params).await },
            "query_fields",
        )
        .await
    }

    pub async fn query_named_fields_async(
        &self,
        sql: &str,
        params: &[DataField],
    ) -> KnowledgeResult<RowData> {
        let pool = self.pool.clone();
        let sql = sql.to_string();
        let params = params.to_vec();
        let metadata_scope = self.metadata_scope.clone();
        self.run_task(
            async move { execute_query_named_fields(pool, metadata_scope, sql, params).await },
            "query_named_fields",
        )
        .await
    }

    async fn run_task<T, F>(&self, fut: F, action: &str) -> KnowledgeResult<T>
    where
        T: Send + 'static,
        F: Future<Output = KnowledgeResult<T>> + Send + 'static,
    {
        self.runtime
            .as_ref()
            .expect("mysql runtime available")
            .handle()
            .spawn(fut)
            .await
            .map_err(|err| join_err("mysql", action, err))?
    }

    fn block_on_task<T, F>(&self, fut: F, action: &str) -> KnowledgeResult<T>
    where
        T: Send + 'static,
        F: Future<Output = KnowledgeResult<T>> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        self.runtime
            .as_ref()
            .expect("mysql runtime available")
            .handle()
            .spawn(async move {
                let _ = tx.send(fut.await);
            });
        rx.recv().map_err(|err| {
            KnowledgeReason::from_logic().to_err().with_detail(format!(
                "mysql async task channel failed during {action}: {err}"
            ))
        })?
    }
}

impl Drop for MySqlProvider {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

async fn validate_startup(pool: &Pool) -> KnowledgeResult<()> {
    let mut conn = pool.get_conn().await.map_err(|err| {
        KnowledgeReason::from_conf().to_err().with_detail(format!(
            "mysql startup validation failed to acquire pooled connection: {err}"
        ))
    })?;
    conn.query_drop("SELECT 1")
        .await
        .map_err(|err| validation_err("connection", err))?;
    Ok(())
}

async fn execute_query(
    pool: Pool,
    metadata_scope: MetadataCacheScope,
    sql: String,
) -> KnowledgeResult<Vec<RowData>> {
    let mut conn = pool.get_conn().await.map_err(|err| {
        KnowledgeReason::from_conf().to_err().with_detail(format!(
            "mysql query failed to acquire pooled connection: {err}"
        ))
    })?;
    let mut prepared_stmt = None;
    let col_names = metadata_cache_get_or_try_init_async_for_scope(
        &metadata_scope,
        Some(ProviderKind::Mysql),
        &sql,
        || async {
            let stmt = conn.prep(&sql).await.map_err(|err| {
                KnowledgeReason::from_rule()
                    .to_err()
                    .with_detail(format!("mysql query prepare failed: {err}"))
            })?;
            let names = statement_col_names(&stmt);
            prepared_stmt = Some(stmt);
            Ok(Some(names))
        },
    )
    .await?;
    let rows: Vec<Row> = if let Some(stmt) = prepared_stmt.as_ref() {
        conn.exec(stmt, ()).await
    } else {
        conn.query(&sql).await
    }
    .map_err(|err| {
        KnowledgeReason::from_rule()
            .to_err()
            .with_detail(format!("mysql query failed: {err}"))
    })?;
    rows.into_iter()
        .map(|row| map_row(row, &col_names))
        .collect()
}

async fn execute_query_row(
    pool: Pool,
    metadata_scope: MetadataCacheScope,
    sql: String,
) -> KnowledgeResult<RowData> {
    let mut conn = pool.get_conn().await.map_err(|err| {
        KnowledgeReason::from_conf().to_err().with_detail(format!(
            "mysql query_row failed to acquire pooled connection: {err}"
        ))
    })?;
    let mut prepared_stmt = None;
    let col_names = metadata_cache_get_or_try_init_async_for_scope(
        &metadata_scope,
        Some(ProviderKind::Mysql),
        &sql,
        || async {
            let stmt = conn.prep(&sql).await.map_err(|err| {
                KnowledgeReason::from_rule()
                    .to_err()
                    .with_detail(format!("mysql query_row prepare failed: {err}"))
            })?;
            let names = statement_col_names(&stmt);
            prepared_stmt = Some(stmt);
            Ok(Some(names))
        },
    )
    .await?;
    let row: Option<Row> = if let Some(stmt) = prepared_stmt.as_ref() {
        conn.exec_first(stmt, ()).await
    } else {
        conn.query_first(&sql).await
    }
    .map_err(|err| {
        KnowledgeReason::from_rule()
            .to_err()
            .with_detail(format!("mysql query_row failed: {err}"))
    })?;
    match row {
        Some(row) => map_row(row, &col_names),
        None => Ok(Vec::new()),
    }
}

async fn execute_query_fields(
    pool: Pool,
    metadata_scope: MetadataCacheScope,
    sql: String,
    params: Vec<DataField>,
) -> KnowledgeResult<Vec<RowData>> {
    let mut conn = pool.get_conn().await.map_err(|err| {
        KnowledgeReason::from_conf().to_err().with_detail(format!(
            "mysql query_fields failed to acquire pooled connection: {err}"
        ))
    })?;
    let bind_params = mysql_named_params(&params);
    let mut prepared_stmt = None;
    let col_names = metadata_cache_get_or_try_init_async_for_scope(
        &metadata_scope,
        Some(ProviderKind::Mysql),
        &sql,
        || async {
            let stmt = conn.prep(&sql).await.map_err(|err| {
                KnowledgeReason::from_rule()
                    .to_err()
                    .with_detail(format!("mysql query_fields prepare failed: {err}"))
            })?;
            let names = statement_col_names(&stmt);
            prepared_stmt = Some(stmt);
            Ok(Some(names))
        },
    )
    .await?;
    let rows: Vec<Row> = if let Some(stmt) = prepared_stmt.as_ref() {
        conn.exec(stmt, bind_params.clone()).await
    } else {
        conn.exec(&sql, bind_params).await
    }
    .map_err(|err| {
        KnowledgeReason::from_rule()
            .to_err()
            .with_detail(format!("mysql query_fields failed: {err}"))
    })?;
    rows.into_iter()
        .map(|row| map_row(row, &col_names))
        .collect()
}

async fn execute_query_named_fields(
    pool: Pool,
    metadata_scope: MetadataCacheScope,
    sql: String,
    params: Vec<DataField>,
) -> KnowledgeResult<RowData> {
    let mut conn = pool.get_conn().await.map_err(|err| {
        KnowledgeReason::from_conf().to_err().with_detail(format!(
            "mysql query_named_fields failed to acquire pooled connection: {err}"
        ))
    })?;
    let bind_params = mysql_named_params(&params);
    let mut prepared_stmt = None;
    let col_names = metadata_cache_get_or_try_init_async_for_scope(
        &metadata_scope,
        Some(ProviderKind::Mysql),
        &sql,
        || async {
            let stmt = conn.prep(&sql).await.map_err(|err| {
                KnowledgeReason::from_rule()
                    .to_err()
                    .with_detail(format!("mysql query_named_fields prepare failed: {err}"))
            })?;
            let names = statement_col_names(&stmt);
            prepared_stmt = Some(stmt);
            Ok(Some(names))
        },
    )
    .await?;
    let row: Option<Row> = if let Some(stmt) = prepared_stmt.as_ref() {
        conn.exec_first(stmt, bind_params.clone()).await
    } else {
        conn.exec_first(&sql, bind_params).await
    }
    .map_err(|err| {
        KnowledgeReason::from_rule()
            .to_err()
            .with_detail(format!("mysql query_named_fields failed: {err}"))
    })?;
    match row {
        Some(row) => map_row(row, &col_names),
        None => Ok(Vec::new()),
    }
}

fn validation_err(stage: &str, err: ::mysql_async::Error) -> wp_error::KnowledgeError {
    KnowledgeReason::from_conf().to_err().with_detail(format!(
        "mysql startup validation failed during {stage}: connection issue: {err}"
    ))
}

fn join_err(provider: &str, action: &str, err: tokio::task::JoinError) -> wp_error::KnowledgeError {
    KnowledgeReason::from_logic().to_err().with_detail(format!(
        "{provider} async task join failed during {action}: {err}"
    ))
}

fn mysql_named_params(params: &[DataField]) -> Params {
    Params::Named(
        params
            .iter()
            .map(|field| (normalize_param_name(field.get_name()), mysql_value(field)))
            .collect(),
    )
}

fn normalize_param_name(name: &str) -> Vec<u8> {
    name.trim_start_matches(':').as_bytes().to_vec()
}

fn mysql_value(field: &DataField) -> MySqlValue {
    match field.get_value() {
        Value::Bool(value) => MySqlValue::Int(if *value { 1 } else { 0 }),
        Value::Digit(value) => MySqlValue::Int(*value),
        Value::Float(value) => MySqlValue::Double(*value),
        Value::Null | Value::Ignore(_) => MySqlValue::NULL,
        Value::Chars(value) => MySqlValue::Bytes(value.to_string().into_bytes()),
        Value::Symbol(value) => MySqlValue::Bytes(value.to_string().into_bytes()),
        Value::Time(value) => MySqlValue::Bytes(value.to_string().into_bytes()),
        Value::Hex(value) => MySqlValue::Bytes(value.to_string().into_bytes()),
        Value::IpNet(value) => MySqlValue::Bytes(value.to_string().into_bytes()),
        Value::IpAddr(value) => MySqlValue::Bytes(value.to_string().into_bytes()),
        Value::Obj(value) => MySqlValue::Bytes(format!("{:?}", value).into_bytes()),
        Value::Array(value) => MySqlValue::Bytes(format!("{:?}", value).into_bytes()),
        Value::Domain(value) => MySqlValue::Bytes(value.0.to_string().into_bytes()),
        Value::Url(value) => MySqlValue::Bytes(value.0.to_string().into_bytes()),
        Value::Email(value) => MySqlValue::Bytes(value.0.to_string().into_bytes()),
        Value::IdCard(value) => MySqlValue::Bytes(value.0.to_string().into_bytes()),
        Value::MobilePhone(value) => MySqlValue::Bytes(value.0.to_string().into_bytes()),
    }
}

fn map_row(row: Row, col_names: &[String]) -> KnowledgeResult<RowData> {
    let mut result = Vec::with_capacity(row.len());
    for (idx, column) in row.columns_ref().iter().enumerate() {
        let name = col_names
            .get(idx)
            .cloned()
            .unwrap_or_else(|| column.name_str().into_owned());
        let field = match row.as_ref(idx) {
            Some(value) => map_value(&name, value),
            None => DataField::new(DataType::default(), &name, Value::Null),
        };
        result.push(field);
    }
    Ok(result)
}

fn statement_col_names(stmt: &Statement) -> Vec<String> {
    stmt.columns()
        .iter()
        .map(|col| col.name_str().into_owned())
        .collect()
}

fn map_value(name: &str, value: &MySqlValue) -> DataField {
    match value {
        MySqlValue::NULL => DataField::new(DataType::default(), name, Value::Null),
        MySqlValue::Int(value) => DataField::from_digit(name, *value),
        MySqlValue::UInt(value) => {
            if *value <= i64::MAX as u64 {
                DataField::from_digit(name, *value as i64)
            } else {
                DataField::from_chars(name, value.to_string())
            }
        }
        MySqlValue::Float(value) => DataField::from_float(name, f64::from(*value)),
        MySqlValue::Double(value) => DataField::from_float(name, *value),
        MySqlValue::Bytes(value) => {
            DataField::from_chars(name, String::from_utf8_lossy(value).to_string())
        }
        MySqlValue::Date(year, month, day, hour, minute, second, micros) => DataField::from_chars(
            name,
            format_mysql_date(*year, *month, *day, *hour, *minute, *second, *micros),
        ),
        MySqlValue::Time(is_neg, days, hours, minutes, seconds, micros) => DataField::from_chars(
            name,
            format_mysql_time(*is_neg, *days, *hours, *minutes, *seconds, *micros),
        ),
    }
}

fn format_mysql_date(
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    micros: u32,
) -> String {
    if hour == 0 && minute == 0 && second == 0 && micros == 0 {
        format!("{year:04}-{month:02}-{day:02}")
    } else if micros == 0 {
        format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
    } else {
        format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{micros:06}")
    }
}

fn format_mysql_time(
    is_neg: bool,
    days: u32,
    hours: u8,
    minutes: u8,
    seconds: u8,
    micros: u32,
) -> String {
    let total_hours = days * 24 + u32::from(hours);
    let sign = if is_neg { "-" } else { "" };
    if micros == 0 {
        format!("{sign}{total_hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{sign}{total_hours:02}:{minutes:02}:{seconds:02}.{micros:06}")
    }
}
