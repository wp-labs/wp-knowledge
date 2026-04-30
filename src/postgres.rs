use std::collections::HashMap;
use std::future::Future;

use crate::error::{KnowledgeResult, Reason};
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use orion_error::UvsFrom;
use orion_error::conversion::ToStructError;
use sqlx::postgres::{PgArguments, PgColumn, PgPoolOptions, PgRow, types::Oid};
use sqlx::{Column, Executor, Pool, Postgres, Row, TypeInfo, ValueRef};
use tokio::runtime::Runtime;
use wp_model_core::model::{DataField, DataType, Value};

use crate::field_format::{bytes_to_prefixed_hex, chars_field, display_chars_field};
use crate::loader::ProviderKind;
use crate::mem::{RowData, query_util::metadata_cache_get_or_try_init_async_for_scope_typed};
use crate::pool_config::CommonPoolConfig as PostgresPoolConfig;
use crate::provider_runtime;
use crate::runtime::MetadataCacheScope;

#[derive(Debug, Clone)]
pub struct PostgresProviderConfig {
    connection_uri: String,
    pool: PostgresPoolConfig,
}

impl PostgresProviderConfig {
    pub fn new(connection_uri: impl Into<String>) -> Self {
        Self {
            connection_uri: connection_uri.into(),
            pool: PostgresPoolConfig::default(),
        }
    }

    pub fn connection_uri(&self) -> &str {
        &self.connection_uri
    }

    pub(crate) fn pool(&self) -> &PostgresPoolConfig {
        &self.pool
    }

    pub fn with_pool_size(mut self, pool_size: Option<u32>) -> Self {
        self.pool = self.pool.with_pool_size(pool_size);
        self
    }

    pub fn with_min_connections(mut self, min_connections: Option<u32>) -> Self {
        self.pool = self.pool.with_min_connections(min_connections);
        self
    }

    pub fn with_acquire_timeout_ms(mut self, timeout_ms: Option<u64>) -> Self {
        self.pool = self.pool.with_acquire_timeout_ms(timeout_ms);
        self
    }

    pub fn with_idle_timeout_ms(mut self, timeout_ms: Option<u64>) -> Self {
        self.pool = self.pool.with_idle_timeout_ms(timeout_ms);
        self
    }

    pub fn with_max_lifetime_ms(mut self, timeout_ms: Option<u64>) -> Self {
        self.pool = self.pool.with_max_lifetime_ms(timeout_ms);
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
        let pool_config = config
            .pool()
            .clone()
            .with_pool_size(Some(config.pool().pool_size().max(1)));
        let pool_size = pool_config.pool_size();
        let connection_uri = config.connection_uri().to_string();
        provider_runtime::init_provider_runtime(
            "postgres",
            "wp-kdb-pg-init",
            "wp-kdb-pg",
            pool_size,
            move |runtime| {
                let metadata_scope_for_thread = metadata_scope;
                let pool = runtime.block_on(async {
                    let pool = PgPoolOptions::new()
                        .max_connections(pool_size)
                        .min_connections(pool_config.min_connections())
                        .acquire_timeout(pool_config.acquire_timeout())
                        .idle_timeout(pool_config.idle_timeout())
                        .max_lifetime(pool_config.max_lifetime())
                        .connect(&connection_uri)
                        .await
                        .map_err(|err| {
                            Reason::from_conf()
                                .to_err()
                                .with_detail(format!("create postgres pool failed: {err}"))
                        })?;
                    validate_startup(&pool).await?;
                    Ok::<Pool<Postgres>, crate::error::KnowledgeError>(pool)
                })?;
                Ok(Self {
                    runtime: Some(runtime),
                    pool,
                    metadata_scope: metadata_scope_for_thread,
                })
            },
        )
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
        provider_runtime::run_task(
            self.runtime.as_ref().expect("postgres runtime available"),
            "postgres",
            action,
            fut,
        )
        .await
    }

    fn block_on_task<T, F>(&self, fut: F, action: &str) -> KnowledgeResult<T>
    where
        T: Send + 'static,
        F: Future<Output = KnowledgeResult<T>> + Send + 'static,
    {
        provider_runtime::block_on_task(
            self.runtime.as_ref().expect("postgres runtime available"),
            "postgres",
            action,
            fut,
        )
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
                Reason::from_rule()
                    .to_err()
                    .with_detail(format!("postgres describe failed: {err}"))
            })?;
            Ok::<Option<Vec<String>>, crate::error::KnowledgeError>(Some(
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

async fn postgres_col_names_from_row_or_describe(
    pool: &Pool<Postgres>,
    metadata_scope: &MetadataCacheScope,
    cache_sql: &str,
    exec_sql: &str,
    row: Option<&PgRow>,
) -> KnowledgeResult<Vec<String>> {
    if let Some(row) = row {
        let names: Vec<String> = row
            .columns()
            .iter()
            .map(|col| col.name().to_string())
            .collect();
        return metadata_cache_get_or_try_init_async_for_scope_typed(
            metadata_scope,
            Some(ProviderKind::Postgres),
            cache_sql,
            || async move { Ok::<Option<Vec<String>>, crate::error::KnowledgeError>(Some(names)) },
        )
        .await;
    }

    postgres_col_names(pool, metadata_scope, cache_sql, exec_sql).await
}

async fn execute_query(
    pool: Pool<Postgres>,
    metadata_scope: &MetadataCacheScope,
    sql: &str,
) -> KnowledgeResult<Vec<RowData>> {
    let rows = sqlx::query::<Postgres>(sql)
        .fetch_all(&pool)
        .await
        .map_err(|err| {
            Reason::from_rule()
                .to_err()
                .with_detail(format!("postgres query failed: {err}"))
        })?;
    let col_names =
        postgres_col_names_from_row_or_describe(&pool, metadata_scope, sql, sql, rows.first())
            .await?;
    rows.iter().map(|row| map_row(row, &col_names)).collect()
}

async fn execute_query_row(
    pool: Pool<Postgres>,
    metadata_scope: &MetadataCacheScope,
    sql: &str,
) -> KnowledgeResult<RowData> {
    let row = sqlx::query::<Postgres>(sql)
        .fetch_optional(&pool)
        .await
        .map_err(|err| {
            Reason::from_rule()
                .to_err()
                .with_detail(format!("postgres query_row failed: {err}"))
        })?;
    let col_names =
        postgres_col_names_from_row_or_describe(&pool, metadata_scope, sql, sql, row.as_ref())
            .await?;
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
    let query = ordered_params
        .iter()
        .fold(sqlx::query::<Postgres>(&rewritten_sql), |query, field| {
            bind_postgres_field(query, field)
        });
    let rows = query.fetch_all(&pool).await.map_err(|err| {
        Reason::from_rule()
            .to_err()
            .with_detail(format!("postgres query_fields failed: {err}"))
    })?;
    let col_names = postgres_col_names_from_row_or_describe(
        &pool,
        metadata_scope,
        sql,
        &rewritten_sql,
        rows.first(),
    )
    .await?;
    rows.iter().map(|row| map_row(row, &col_names)).collect()
}

async fn execute_query_named_fields(
    pool: Pool<Postgres>,
    metadata_scope: &MetadataCacheScope,
    sql: &str,
    params: &[DataField],
) -> KnowledgeResult<RowData> {
    let (rewritten_sql, ordered_params) = rewrite_sql(sql, params)?;
    let query = ordered_params
        .iter()
        .fold(sqlx::query::<Postgres>(&rewritten_sql), |query, field| {
            bind_postgres_field(query, field)
        });
    let row = query.fetch_optional(&pool).await.map_err(|err| {
        Reason::from_rule()
            .to_err()
            .with_detail(format!("postgres query_named_fields failed: {err}"))
    })?;
    let col_names = postgres_col_names_from_row_or_describe(
        &pool,
        metadata_scope,
        sql,
        &rewritten_sql,
        row.as_ref(),
    )
    .await?;
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

fn validation_err(stage: &str, err: sqlx::Error) -> crate::error::KnowledgeError {
    Reason::from_conf().to_err().with_detail(format!(
        "postgres startup validation failed during {stage}: connection issue: {err}"
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
                    Reason::from_rule()
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
    if let Some(field) = decode_postgres_boolean(row, idx, name, &ty)? {
        return Ok(field);
    }

    if let Some(field) = decode_postgres_integer(row, idx, name, &ty)? {
        return Ok(field);
    }

    if let Some(field) = decode_postgres_float(row, idx, name, &ty)? {
        return Ok(field);
    }

    if let Some(field) = decode_postgres_decimal(row, idx, name, &ty)? {
        return Ok(field);
    }

    if let Some(field) = decode_postgres_network(row, idx, name, &ty)? {
        return Ok(field);
    }

    if let Some(field) = decode_postgres_binary(row, idx, name, &ty)? {
        return Ok(field);
    }

    if let Some(field) = decode_postgres_text(row, idx, name, &ty)? {
        return Ok(field);
    }

    if let Some(field) = decode_postgres_temporal(row, idx, name, &ty)? {
        return Ok(field);
    }

    if let Some(field) = decode_postgres_json(row, idx, name, &ty)? {
        return Ok(field);
    }

    row.try_get::<String, _>(idx)
        .map(|value| DataField::from_chars(name.to_string(), value))
        .map_err(pg_decode_err)
}

fn decode_postgres_boolean(
    row: &PgRow,
    idx: usize,
    name: &str,
    ty: &str,
) -> KnowledgeResult<Option<DataField>> {
    if ty != "BOOL" {
        return Ok(None);
    }

    row.try_get::<bool, _>(idx)
        .map(|value| DataField::from_bool(name.to_string(), value))
        .map(Some)
        .map_err(pg_decode_err)
}

fn decode_postgres_integer(
    row: &PgRow,
    idx: usize,
    name: &str,
    ty: &str,
) -> KnowledgeResult<Option<DataField>> {
    let field = match ty {
        "INT2" => row
            .try_get::<i16, _>(idx)
            .map(|value| DataField::from_digit(name.to_string(), i64::from(value)))
            .map_err(pg_decode_err)?,
        "INT4" => row
            .try_get::<i32, _>(idx)
            .map(|value| DataField::from_digit(name.to_string(), i64::from(value)))
            .map_err(pg_decode_err)?,
        "INT8" => row
            .try_get::<i64, _>(idx)
            .map(|value| DataField::from_digit(name.to_string(), value))
            .map_err(pg_decode_err)?,
        "OID" => row
            .try_get::<Oid, _>(idx)
            .map(|value| DataField::from_digit(name.to_string(), i64::from(value.0)))
            .map_err(pg_decode_err)?,
        _ => return Ok(None),
    };

    Ok(Some(field))
}

fn decode_postgres_float(
    row: &PgRow,
    idx: usize,
    name: &str,
    ty: &str,
) -> KnowledgeResult<Option<DataField>> {
    let field = match ty {
        "FLOAT4" => row
            .try_get::<f32, _>(idx)
            .map(|value| DataField::from_float(name.to_string(), f64::from(value)))
            .map_err(pg_decode_err)?,
        "FLOAT8" => row
            .try_get::<f64, _>(idx)
            .map(|value| DataField::from_float(name.to_string(), value))
            .map_err(pg_decode_err)?,
        _ => return Ok(None),
    };

    Ok(Some(field))
}

fn decode_postgres_decimal(
    row: &PgRow,
    idx: usize,
    name: &str,
    ty: &str,
) -> KnowledgeResult<Option<DataField>> {
    if ty != "NUMERIC" {
        return Ok(None);
    }

    row.try_get::<sqlx::types::BigDecimal, _>(idx)
        .map(|value| display_chars_field(name.to_string(), value))
        .map(Some)
        .map_err(pg_decode_err)
}

fn decode_postgres_network(
    row: &PgRow,
    idx: usize,
    name: &str,
    ty: &str,
) -> KnowledgeResult<Option<DataField>> {
    match ty {
        "UUID" => row
            .try_get::<sqlx::types::Uuid, _>(idx)
            .map(|value| chars_field(name.to_string(), value.to_string()))
            .map(Some)
            .map_err(pg_decode_err),
        "INET" | "CIDR" => row
            .try_get::<sqlx::types::ipnet::IpNet, _>(idx)
            .map(|value| chars_field(name.to_string(), value.to_string()))
            .map(Some)
            .map_err(pg_decode_err),
        _ => Ok(None),
    }
}

fn decode_postgres_binary(
    row: &PgRow,
    idx: usize,
    name: &str,
    ty: &str,
) -> KnowledgeResult<Option<DataField>> {
    if ty != "BYTEA" {
        return Ok(None);
    }

    row.try_get::<Vec<u8>, _>(idx)
        .map(|value| chars_field(name.to_string(), bytes_to_prefixed_hex(&value)))
        .map(Some)
        .map_err(pg_decode_err)
}

fn decode_postgres_text(
    row: &PgRow,
    idx: usize,
    name: &str,
    ty: &str,
) -> KnowledgeResult<Option<DataField>> {
    if !matches!(ty, "VARCHAR" | "TEXT" | "BPCHAR" | "NAME" | "UNKNOWN") {
        return Ok(None);
    }

    row.try_get::<String, _>(idx)
        .map(|value| chars_field(name.to_string(), value))
        .map(Some)
        .map_err(pg_decode_err)
}

fn decode_postgres_temporal(
    row: &PgRow,
    idx: usize,
    name: &str,
    ty: &str,
) -> KnowledgeResult<Option<DataField>> {
    let field = match ty {
        "TIMESTAMP" => row
            .try_get::<NaiveDateTime, _>(idx)
            .map(|value| display_chars_field(name.to_string(), value))
            .map_err(pg_decode_err)?,
        "TIMESTAMPTZ" => row
            .try_get::<DateTime<Utc>, _>(idx)
            .map(|value| chars_field(name.to_string(), value.to_rfc3339()))
            .map_err(pg_decode_err)?,
        "DATE" => row
            .try_get::<NaiveDate, _>(idx)
            .map(|value| display_chars_field(name.to_string(), value))
            .map_err(pg_decode_err)?,
        "TIME" => row
            .try_get::<NaiveTime, _>(idx)
            .map(|value| display_chars_field(name.to_string(), value))
            .map_err(pg_decode_err)?,
        _ => return Ok(None),
    };

    Ok(Some(field))
}

fn decode_postgres_json(
    row: &PgRow,
    idx: usize,
    name: &str,
    ty: &str,
) -> KnowledgeResult<Option<DataField>> {
    if !matches!(ty, "JSON" | "JSONB") {
        return Ok(None);
    }

    row.try_get::<serde_json::Value, _>(idx)
        .map(|value| display_chars_field(name.to_string(), value))
        .map(Some)
        .map_err(pg_decode_err)
}

fn pg_decode_err(err: sqlx::Error) -> crate::error::KnowledgeError {
    Reason::from_rule()
        .to_err()
        .with_detail(format!("postgres row decode failed: {err}"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::field_format::bytes_to_hex;
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

    #[test]
    fn bytes_to_hex_formats_lowercase_hex() {
        assert_eq!(bytes_to_hex(&[0x00, 0xff, 0x10, 0xaa]), "00ff10aa");
    }

    #[test]
    fn postgres_pool_config_clamps_min_connections_and_handles_zero_timeouts() {
        let pool = PostgresPoolConfig::default()
            .with_pool_size(Some(4))
            .with_min_connections(Some(8))
            .with_idle_timeout_ms(Some(0))
            .with_max_lifetime_ms(Some(0))
            .with_acquire_timeout_ms(Some(2500));
        assert_eq!(pool.pool_size(), 4);
        assert_eq!(pool.min_connections(), 4);
        assert_eq!(pool.acquire_timeout(), Duration::from_millis(2500));
        assert_eq!(pool.idle_timeout(), None);
        assert_eq!(pool.max_lifetime(), None);
    }
}
