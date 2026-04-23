use std::collections::HashMap;
use std::future::Future;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use orion_error::{ToStructError, UvsFrom};
use sqlx::postgres::{PgArguments, PgColumn, PgPoolOptions, PgRow, types::Oid};
use sqlx::{Column, Executor, Pool, Postgres, Row, TypeInfo, ValueRef};
use tokio::runtime::{Builder, Runtime};
use wp_error::{KnowledgeReason, KnowledgeResult};
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
    pool: Pool<Postgres>,
    metadata_scope: MetadataCacheScope,
}

impl PostgresProvider {
    pub fn connect(
        config: &PostgresProviderConfig,
        metadata_scope: MetadataCacheScope,
    ) -> KnowledgeResult<Self> {
        let pool_size = config.pool_size().max(1);
        let connection_uri = config.connection_uri().to_string();
        let metadata_scope_for_thread = metadata_scope.clone();
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("wp-kdb-pg-init".to_string())
            .spawn(move || {
                let runtime = Builder::new_multi_thread()
                    .worker_threads(pool_size as usize)
                    .enable_all()
                    .thread_name("wp-kdb-pg")
                    .build()
                    .map_err(|err| {
                        KnowledgeReason::from_conf()
                            .to_err()
                            .with_detail(format!("create postgres tokio runtime failed: {err}"))
                    });
                let result = runtime.and_then(|runtime| {
                    let pool = runtime.block_on(async {
                        let pool = PgPoolOptions::new()
                            .max_connections(pool_size)
                            .min_connections(0)
                            .idle_timeout(Some(Duration::from_secs(60)))
                            .connect(&connection_uri)
                            .await
                            .map_err(|err| {
                                KnowledgeReason::from_conf()
                                    .to_err()
                                    .with_detail(format!("create postgres pool failed: {err}"))
                            })?;
                        validate_startup(&pool).await?;
                        Ok::<Pool<Postgres>, wp_error::KnowledgeError>(pool)
                    })?;
                    Ok::<PostgresProvider, wp_error::KnowledgeError>(Self {
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
        let pool = self.pool.clone();
        let sql = sql.to_string();
        let metadata_scope = self.metadata_scope.clone();
        self.block_on_task(
            async move { execute_query(pool, &metadata_scope, &sql).await },
            "query",
        )
    }

    pub fn query_row(&self, sql: &str) -> KnowledgeResult<RowData> {
        let pool = self.pool.clone();
        let sql = sql.to_string();
        let metadata_scope = self.metadata_scope.clone();
        self.block_on_task(
            async move { execute_query_row(pool, &metadata_scope, &sql).await },
            "query_row",
        )
    }

    pub fn query_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<Vec<RowData>> {
        let pool = self.pool.clone();
        let sql = sql.to_string();
        let params = params.to_vec();
        let metadata_scope = self.metadata_scope.clone();
        self.block_on_task(
            async move { execute_query_fields(pool, &metadata_scope, &sql, &params).await },
            "query_fields",
        )
    }

    pub fn query_named_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<RowData> {
        let pool = self.pool.clone();
        let sql = sql.to_string();
        let params = params.to_vec();
        let metadata_scope = self.metadata_scope.clone();
        self.block_on_task(
            async move { execute_query_named_fields(pool, &metadata_scope, &sql, &params).await },
            "query_named_fields",
        )
    }

    pub async fn query_async(&self, sql: &str) -> KnowledgeResult<Vec<RowData>> {
        let pool = self.pool.clone();
        let sql = sql.to_string();
        let metadata_scope = self.metadata_scope.clone();
        self.run_task(
            async move { execute_query(pool, &metadata_scope, &sql).await },
            "query",
        )
        .await
    }

    pub async fn query_row_async(&self, sql: &str) -> KnowledgeResult<RowData> {
        let pool = self.pool.clone();
        let sql = sql.to_string();
        let metadata_scope = self.metadata_scope.clone();
        self.run_task(
            async move { execute_query_row(pool, &metadata_scope, &sql).await },
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
            async move { execute_query_fields(pool, &metadata_scope, &sql, &params).await },
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
            async move { execute_query_named_fields(pool, &metadata_scope, &sql, &params).await },
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

async fn validate_startup(pool: &Pool<Postgres>) -> KnowledgeResult<()> {
    sqlx::query::<Postgres>("SELECT 1")
        .execute(pool)
        .await
        .map_err(|err| validation_err("connection", err))?;
    Ok(())
}

async fn postgres_col_names(
    pool: &Pool<Postgres>,
    metadata_scope: &MetadataCacheScope,
    cache_sql: &str,
    exec_sql: &str,
) -> KnowledgeResult<Vec<String>> {
    metadata_cache_get_or_try_init_async_for_scope_typed(
        metadata_scope,
        Some(ProviderKind::Postgres),
        cache_sql,
        || async {
            let describe = pool.describe(exec_sql).await.map_err(|err| {
                KnowledgeReason::from_rule()
                    .to_err()
                    .with_detail(format!("postgres describe failed: {err}"))
            })?;
            Ok::<Option<Vec<String>>, wp_error::KnowledgeError>(Some(
                describe
                    .columns()
                    .iter()
                    .map(|col: &PgColumn| col.name().to_string())
                    .collect(),
            ))
        },
    )
    .await
}

async fn execute_query(
    pool: Pool<Postgres>,
    metadata_scope: &MetadataCacheScope,
    sql: &str,
) -> KnowledgeResult<Vec<RowData>> {
    let col_names = postgres_col_names(&pool, metadata_scope, sql, sql).await?;
    let rows = sqlx::query::<Postgres>(sql)
        .fetch_all(&pool)
        .await
        .map_err(|err| {
            KnowledgeReason::from_rule()
                .to_err()
                .with_detail(format!("postgres query failed: {err}"))
        })?;
    rows.iter().map(|row| map_row(row, &col_names)).collect()
}

async fn execute_query_row(
    pool: Pool<Postgres>,
    metadata_scope: &MetadataCacheScope,
    sql: &str,
) -> KnowledgeResult<RowData> {
    let col_names = postgres_col_names(&pool, metadata_scope, sql, sql).await?;
    let row = sqlx::query::<Postgres>(sql)
        .fetch_optional(&pool)
        .await
        .map_err(|err| {
            KnowledgeReason::from_rule()
                .to_err()
                .with_detail(format!("postgres query_row failed: {err}"))
        })?;
    if let Some(row) = row.as_ref() {
        map_row(row, &col_names)
    } else {
        Ok(Vec::new())
    }
}

async fn execute_query_fields(
    pool: Pool<Postgres>,
    metadata_scope: &MetadataCacheScope,
    sql: &str,
    params: &[DataField],
) -> KnowledgeResult<Vec<RowData>> {
    let (rewritten_sql, ordered_params) = rewrite_sql(sql, params)?;
    let col_names = postgres_col_names(&pool, metadata_scope, sql, &rewritten_sql).await?;
    let query = ordered_params
        .iter()
        .fold(sqlx::query::<Postgres>(&rewritten_sql), |query, field| {
            bind_postgres_field(query, field)
        });
    let rows = query.fetch_all(&pool).await.map_err(|err| {
        KnowledgeReason::from_rule()
            .to_err()
            .with_detail(format!("postgres query_fields failed: {err}"))
    })?;
    rows.iter().map(|row| map_row(row, &col_names)).collect()
}

async fn execute_query_named_fields(
    pool: Pool<Postgres>,
    metadata_scope: &MetadataCacheScope,
    sql: &str,
    params: &[DataField],
) -> KnowledgeResult<RowData> {
    let (rewritten_sql, ordered_params) = rewrite_sql(sql, params)?;
    let col_names = postgres_col_names(&pool, metadata_scope, sql, &rewritten_sql).await?;
    let query = ordered_params
        .iter()
        .fold(sqlx::query::<Postgres>(&rewritten_sql), |query, field| {
            bind_postgres_field(query, field)
        });
    let row = query.fetch_optional(&pool).await.map_err(|err| {
        KnowledgeReason::from_rule()
            .to_err()
            .with_detail(format!("postgres query_named_fields failed: {err}"))
    })?;
    if let Some(row) = row.as_ref() {
        map_row(row, &col_names)
    } else {
        Ok(Vec::new())
    }
}

fn bind_postgres_field<'q>(
    query: sqlx::query::Query<'q, Postgres, PgArguments>,
    field: &DataField,
) -> sqlx::query::Query<'q, Postgres, PgArguments> {
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

fn validation_err(stage: &str, err: sqlx::Error) -> wp_error::KnowledgeError {
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

fn map_row(row: &PgRow, col_names: &[String]) -> KnowledgeResult<RowData> {
    let mut out = Vec::with_capacity(row.len());
    for (idx, col) in row.columns().iter().enumerate() {
        let col_name = col_names
            .get(idx)
            .map(|name| name.as_str())
            .unwrap_or(col.name());
        out.push(map_value(row, idx, col_name)?);
    }
    Ok(out)
}

fn map_value(row: &PgRow, idx: usize, name: &str) -> KnowledgeResult<DataField> {
    let raw = row.try_get_raw(idx).map_err(pg_decode_err)?;
    if raw.is_null() {
        return Ok(DataField::new(DataType::default(), name, Value::Null));
    }

    let ty = row.column(idx).type_info().name().to_ascii_uppercase();
    match ty.as_str() {
        "BOOL" => Ok(row
            .try_get::<bool, _>(idx)
            .map(|value| DataField::from_bool(name.to_string(), value))
            .map_err(pg_decode_err)?),
        "INT2" => Ok(row
            .try_get::<i16, _>(idx)
            .map(|value| DataField::from_digit(name.to_string(), i64::from(value)))
            .map_err(pg_decode_err)?),
        "INT4" => Ok(row
            .try_get::<i32, _>(idx)
            .map(|value| DataField::from_digit(name.to_string(), i64::from(value)))
            .map_err(pg_decode_err)?),
        "INT8" => Ok(row
            .try_get::<i64, _>(idx)
            .map(|value| DataField::from_digit(name.to_string(), value))
            .map_err(pg_decode_err)?),
        "OID" => Ok(row
            .try_get::<Oid, _>(idx)
            .map(|value| DataField::from_digit(name.to_string(), i64::from(value.0)))
            .map_err(pg_decode_err)?),
        "FLOAT4" => Ok(row
            .try_get::<f32, _>(idx)
            .map(|value| DataField::from_float(name.to_string(), f64::from(value)))
            .map_err(pg_decode_err)?),
        "FLOAT8" => Ok(row
            .try_get::<f64, _>(idx)
            .map(|value| DataField::from_float(name.to_string(), value))
            .map_err(pg_decode_err)?),
        "NUMERIC" => Ok(row
            .try_get::<sqlx::types::BigDecimal, _>(idx)
            .map(|value| DataField::from_chars(name.to_string(), value.to_string()))
            .map_err(pg_decode_err)?),
        "VARCHAR" | "TEXT" | "BPCHAR" | "NAME" | "UNKNOWN" => Ok(row
            .try_get::<String, _>(idx)
            .map(|value| DataField::from_chars(name.to_string(), value))
            .map_err(pg_decode_err)?),
        "TIMESTAMP" => Ok(row
            .try_get::<NaiveDateTime, _>(idx)
            .map(|value| DataField::from_chars(name.to_string(), value.to_string()))
            .map_err(pg_decode_err)?),
        "TIMESTAMPTZ" => Ok(row
            .try_get::<DateTime<Utc>, _>(idx)
            .map(|value| DataField::from_chars(name.to_string(), value.to_rfc3339()))
            .map_err(pg_decode_err)?),
        "DATE" => Ok(row
            .try_get::<NaiveDate, _>(idx)
            .map(|value| DataField::from_chars(name.to_string(), value.to_string()))
            .map_err(pg_decode_err)?),
        "TIME" => Ok(row
            .try_get::<NaiveTime, _>(idx)
            .map(|value| DataField::from_chars(name.to_string(), value.to_string()))
            .map_err(pg_decode_err)?),
        "JSON" | "JSONB" => Ok(row
            .try_get::<serde_json::Value, _>(idx)
            .map(|value| DataField::from_chars(name.to_string(), value.to_string()))
            .map_err(pg_decode_err)?),
        _ => Ok(row
            .try_get::<String, _>(idx)
            .map(|value| DataField::from_chars(name.to_string(), value))
            .map_err(pg_decode_err)?),
    }
}

fn pg_decode_err(err: sqlx::Error) -> wp_error::KnowledgeError {
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
