use std::future::Future;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use orion_error::{ToStructError, UvsFrom};
use sqlx::mysql::{MySqlArguments, MySqlColumn, MySqlPoolOptions, MySqlRow};
use sqlx::{Column, Executor, MySql, Pool, Row, TypeInfo, ValueRef};
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
    pool: Pool<MySql>,
    metadata_scope: MetadataCacheScope,
}

impl MySqlProvider {
    pub fn connect(
        config: &MySqlProviderConfig,
        metadata_scope: MetadataCacheScope,
    ) -> KnowledgeResult<Self> {
        let pool_size = config.pool_size().max(1);
        let connection_uri = config.connection_uri().to_string();
        let metadata_scope_for_thread = metadata_scope.clone();
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("wp-kdb-mysql-init".to_string())
            .spawn(move || {
                let runtime = Builder::new_multi_thread()
                    .worker_threads(pool_size as usize)
                    .enable_all()
                    .thread_name("wp-kdb-mysql")
                    .build()
                    .map_err(|err| {
                        KnowledgeReason::from_conf()
                            .to_err()
                            .with_detail(format!("create mysql tokio runtime failed: {err}"))
                    });
                let result = runtime.and_then(|runtime| {
                    let pool = runtime.block_on(async {
                        let pool = MySqlPoolOptions::new()
                            .max_connections(pool_size)
                            .min_connections(0)
                            .idle_timeout(Some(Duration::from_secs(60)))
                            .connect(&connection_uri)
                            .await
                            .map_err(|err| {
                                KnowledgeReason::from_conf()
                                    .to_err()
                                    .with_detail(format!("create mysql pool failed: {err}"))
                            })?;
                        validate_startup(&pool).await?;
                        Ok::<Pool<MySql>, wp_error::KnowledgeError>(pool)
                    })?;
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

async fn validate_startup(pool: &Pool<MySql>) -> KnowledgeResult<()> {
    sqlx::query::<MySql>("SELECT 1")
        .execute(pool)
        .await
        .map_err(|err| validation_err("connection", err))?;
    Ok(())
}

async fn mysql_col_names(
    pool: &Pool<MySql>,
    metadata_scope: &MetadataCacheScope,
    cache_sql: &str,
    exec_sql: &str,
) -> KnowledgeResult<Vec<String>> {
    metadata_cache_get_or_try_init_async_for_scope(
        metadata_scope,
        Some(ProviderKind::Mysql),
        cache_sql,
        || async {
            let describe = pool.describe(exec_sql).await.map_err(|err| {
                KnowledgeReason::from_rule()
                    .to_err()
                    .with_detail(format!("mysql describe failed: {err}"))
            })?;
            Ok(Some(
                describe
                    .columns()
                    .iter()
                    .map(|col: &MySqlColumn| col.name().to_string())
                    .collect(),
            ))
        },
    )
    .await
}

async fn execute_query(
    pool: Pool<MySql>,
    metadata_scope: MetadataCacheScope,
    sql: String,
) -> KnowledgeResult<Vec<RowData>> {
    let col_names = mysql_col_names(&pool, &metadata_scope, &sql, &sql).await?;
    let rows = sqlx::query::<MySql>(&sql)
        .fetch_all(&pool)
        .await
        .map_err(|err| {
            KnowledgeReason::from_rule()
                .to_err()
                .with_detail(format!("mysql query failed: {err}"))
        })?;
    rows.iter().map(|row| map_row(row, &col_names)).collect()
}

async fn execute_query_row(
    pool: Pool<MySql>,
    metadata_scope: MetadataCacheScope,
    sql: String,
) -> KnowledgeResult<RowData> {
    let col_names = mysql_col_names(&pool, &metadata_scope, &sql, &sql).await?;
    let row = sqlx::query::<MySql>(&sql)
        .fetch_optional(&pool)
        .await
        .map_err(|err| {
            KnowledgeReason::from_rule()
                .to_err()
                .with_detail(format!("mysql query_row failed: {err}"))
        })?;
    match row {
        Some(row) => map_row(&row, &col_names),
        None => Ok(Vec::new()),
    }
}

async fn execute_query_fields(
    pool: Pool<MySql>,
    metadata_scope: MetadataCacheScope,
    sql: String,
    params: Vec<DataField>,
) -> KnowledgeResult<Vec<RowData>> {
    let (rewritten_sql, ordered_params) = rewrite_sql(&sql, &params)?;
    let col_names = mysql_col_names(&pool, &metadata_scope, &sql, &rewritten_sql).await?;
    let query = ordered_params
        .iter()
        .fold(sqlx::query::<MySql>(&rewritten_sql), |query, field| {
            bind_mysql_field(query, field)
        });
    let rows = query.fetch_all(&pool).await.map_err(|err| {
        KnowledgeReason::from_rule()
            .to_err()
            .with_detail(format!("mysql query_fields failed: {err}"))
    })?;
    rows.iter().map(|row| map_row(row, &col_names)).collect()
}

async fn execute_query_named_fields(
    pool: Pool<MySql>,
    metadata_scope: MetadataCacheScope,
    sql: String,
    params: Vec<DataField>,
) -> KnowledgeResult<RowData> {
    let (rewritten_sql, ordered_params) = rewrite_sql(&sql, &params)?;
    let col_names = mysql_col_names(&pool, &metadata_scope, &sql, &rewritten_sql).await?;
    let query = ordered_params
        .iter()
        .fold(sqlx::query::<MySql>(&rewritten_sql), |query, field| {
            bind_mysql_field(query, field)
        });
    let row = query.fetch_optional(&pool).await.map_err(|err| {
        KnowledgeReason::from_rule()
            .to_err()
            .with_detail(format!("mysql query_named_fields failed: {err}"))
    })?;
    match row {
        Some(row) => map_row(&row, &col_names),
        None => Ok(Vec::new()),
    }
}

fn validation_err(stage: &str, err: sqlx::Error) -> wp_error::KnowledgeError {
    KnowledgeReason::from_conf().to_err().with_detail(format!(
        "mysql startup validation failed during {stage}: connection issue: {err}"
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
    let mut by_name = std::collections::HashMap::with_capacity(params.len());
    for field in params {
        by_name.insert(normalize_param_name(field.get_name()), field);
    }

    let bytes = sql.as_bytes();
    let mut ordered = Vec::new();
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
                        .with_detail(format!("mysql query missing param: {raw_name}"))
                })?;
                ordered.push(*field);
                out.push('?');
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

fn bind_mysql_field<'q>(
    query: sqlx::query::Query<'q, MySql, MySqlArguments>,
    field: &DataField,
) -> sqlx::query::Query<'q, MySql, MySqlArguments> {
    match field.get_value() {
        Value::Bool(value) => query.bind(*value),
        Value::Digit(value) => query.bind(*value),
        Value::Float(value) => query.bind(*value),
        Value::Null | Value::Ignore(_) => query.bind(Option::<String>::None),
        Value::Chars(value) => query.bind(value.to_string()),
        Value::Symbol(value) => query.bind(value.to_string()),
        Value::Time(value) => query.bind(value.to_string()),
        Value::Hex(value) => query.bind(value.to_string()),
        Value::IpNet(value) => query.bind(value.to_string()),
        Value::IpAddr(value) => query.bind(value.to_string()),
        Value::Obj(value) => query.bind(format!("{:?}", value)),
        Value::Array(value) => query.bind(format!("{:?}", value)),
        Value::Domain(value) => query.bind(value.0.to_string()),
        Value::Url(value) => query.bind(value.0.to_string()),
        Value::Email(value) => query.bind(value.0.to_string()),
        Value::IdCard(value) => query.bind(value.0.to_string()),
        Value::MobilePhone(value) => query.bind(value.0.to_string()),
    }
}

fn map_row(row: &MySqlRow, col_names: &[String]) -> KnowledgeResult<RowData> {
    let mut result = Vec::with_capacity(row.len());
    for (idx, column) in row.columns().iter().enumerate() {
        let name = col_names
            .get(idx)
            .cloned()
            .unwrap_or_else(|| column.name().to_string());
        result.push(map_value(row, idx, &name)?);
    }
    Ok(result)
}

fn map_value(row: &MySqlRow, idx: usize, name: &str) -> KnowledgeResult<DataField> {
    let raw = row.try_get_raw(idx).map_err(mysql_decode_err)?;
    if raw.is_null() {
        return Ok(DataField::new(DataType::default(), name, Value::Null));
    }

    let ty = row.column(idx).type_info().name().to_ascii_uppercase();
    let (base_ty, is_unsigned) = mysql_type_base_name(&ty);
    match base_ty {
        "BOOL" | "BOOLEAN" => row
            .try_get::<i8, _>(idx)
            .map(|value| DataField::from_digit(name, i64::from(value)))
            .or_else(|_| {
                row.try_get::<bool, _>(idx)
                    .map(|value| DataField::from_digit(name, if value { 1 } else { 0 }))
            })
            .map_err(mysql_decode_err),
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "INTEGER" | "BIGINT" | "YEAR" => {
            if is_unsigned {
                row.try_get::<u64, _>(idx)
                    .map(|value| {
                        if value <= i64::MAX as u64 {
                            DataField::from_digit(name, value as i64)
                        } else {
                            DataField::from_chars(name, value.to_string())
                        }
                    })
                    .map_err(mysql_decode_err)
            } else {
                row.try_get::<i64, _>(idx)
                    .map(|value| DataField::from_digit(name, value))
                    .map_err(mysql_decode_err)
            }
        }
        "BIT" => row
            .try_get::<u64, _>(idx)
            .map(|value| mysql_bit_u64_to_field(name, value))
            .map_err(mysql_decode_err),
        "FLOAT" | "DOUBLE" | "REAL" => row
            .try_get::<f64, _>(idx)
            .map(|value| DataField::from_float(name, value))
            .map_err(mysql_decode_err),
        "DECIMAL" => row
            .try_get::<sqlx::types::BigDecimal, _>(idx)
            .map(|value| DataField::from_chars(name, value.to_string()))
            .map_err(mysql_decode_err),
        "DATE" => row
            .try_get::<NaiveDate, _>(idx)
            .map(|value| DataField::from_chars(name, value.to_string()))
            .map_err(mysql_decode_err),
        "TIME" => row
            .try_get::<NaiveTime, _>(idx)
            .map(|value| DataField::from_chars(name, value.to_string()))
            .map_err(mysql_decode_err),
        "DATETIME" | "TIMESTAMP" => row
            .try_get::<NaiveDateTime, _>(idx)
            .map(|value| DataField::from_chars(name, value.to_string()))
            .map_err(mysql_decode_err),
        "JSON" => row
            .try_get::<serde_json::Value, _>(idx)
            .map(|value| DataField::from_chars(name, value.to_string()))
            .map_err(mysql_decode_err),
        _ => row
            .try_get::<String, _>(idx)
            .map(|value| DataField::from_chars(name, value))
            .or_else(|_| {
                row.try_get::<Vec<u8>, _>(idx).map(|value| {
                    DataField::from_chars(name, String::from_utf8_lossy(&value).to_string())
                })
            })
            .map_err(mysql_decode_err),
    }
}

fn mysql_type_base_name(ty: &str) -> (&str, bool) {
    let unsigned_ty = ty.strip_suffix(" UNSIGNED");
    (unsigned_ty.unwrap_or(ty), unsigned_ty.is_some())
}

fn mysql_bit_u64_to_field(name: &str, value: u64) -> DataField {
    if value <= i64::MAX as u64 {
        DataField::from_digit(name, value as i64)
    } else {
        DataField::from_chars(name, value.to_string())
    }
}

fn mysql_decode_err(err: sqlx::Error) -> wp_error::KnowledgeError {
    KnowledgeReason::from_rule()
        .to_err()
        .with_detail(format!("mysql row decode failed: {err}"))
}

#[cfg(test)]
mod tests {
    use super::{mysql_bit_u64_to_field, mysql_type_base_name};
    use wp_model_core::model::Value;

    #[test]
    fn mysql_type_base_name_detects_unsigned_integer_types() {
        assert_eq!(mysql_type_base_name("BIGINT UNSIGNED"), ("BIGINT", true));
        assert_eq!(mysql_type_base_name("INT UNSIGNED"), ("INT", true));
        assert_eq!(mysql_type_base_name("BIGINT"), ("BIGINT", false));
    }

    #[test]
    fn mysql_type_base_name_preserves_decimal_and_bit() {
        assert_eq!(mysql_type_base_name("DECIMAL"), ("DECIMAL", false));
        assert_eq!(mysql_type_base_name("BIT"), ("BIT", false));
    }

    #[test]
    fn mysql_bit_u64_to_field_maps_small_bits_to_digit() {
        let field = mysql_bit_u64_to_field("flags", 0b1010);
        assert_eq!(field.get_value(), &Value::Digit(10));
    }

    #[test]
    fn mysql_bit_u64_to_field_maps_large_bits_to_decimal_chars() {
        let field = mysql_bit_u64_to_field("flags", u64::MAX);
        assert_eq!(
            field.get_value(),
            &Value::Chars("18446744073709551615".into())
        );
    }
}
