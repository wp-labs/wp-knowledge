use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, mpsc};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use orion_error::{ToStructError, UvsFrom};
use postgres::types::{ToSql, Type};
use postgres::{Client, NoTls, Row, Statement};
use serde_json::Value as JsonValue;
use wp_error::{KnowledgeReason, KnowledgeResult};
use wp_model_core::model::{DataField, DataType, Value};

use crate::mem::{RowData, query_util::metadata_cache_get_or_try_init};

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
    workers: Vec<PostgresWorkerHandle>,
    next_worker: AtomicUsize,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

#[derive(Debug)]
struct PostgresWorkerHandle {
    tx: mpsc::Sender<PostgresCommand>,
}

enum PostgresCommand {
    Query {
        sql: String,
        reply: mpsc::Sender<KnowledgeResult<Vec<RowData>>>,
    },
    QueryRow {
        sql: String,
        reply: mpsc::Sender<KnowledgeResult<RowData>>,
    },
    QueryFields {
        sql: String,
        params: Vec<DataField>,
        reply: mpsc::Sender<KnowledgeResult<Vec<RowData>>>,
    },
    Shutdown,
}

impl PostgresWorkerHandle {
    fn send(&self, cmd: PostgresCommand) -> KnowledgeResult<()> {
        self.tx.send(cmd).map_err(|err| {
            KnowledgeReason::from_conf()
                .to_err()
                .with_detail(format!("postgres worker channel send failed: {err}"))
        })
    }
}

impl PostgresProvider {
    pub fn connect(config: &PostgresProviderConfig) -> KnowledgeResult<Self> {
        let worker_count = config.pool_size().max(1) as usize;
        let mut workers = Vec::with_capacity(worker_count);
        let mut threads = Vec::with_capacity(worker_count);

        for worker_idx in 0..worker_count {
            match spawn_worker(config.connection_uri(), worker_idx) {
                Ok((worker, thread)) => {
                    workers.push(worker);
                    threads.push(thread);
                }
                Err(err) => {
                    for worker in &workers {
                        let _ = worker.send(PostgresCommand::Shutdown);
                    }
                    for thread in threads {
                        let _ = thread.join();
                    }
                    return Err(err);
                }
            }
        }

        Ok(Self {
            workers,
            next_worker: AtomicUsize::new(0),
            threads: Mutex::new(threads),
        })
    }

    pub fn query(&self, sql: &str) -> KnowledgeResult<Vec<RowData>> {
        let worker = self.pick_worker()?;
        let (reply_tx, reply_rx) = mpsc::channel();
        worker.send(PostgresCommand::Query {
            sql: sql.to_string(),
            reply: reply_tx,
        })?;
        recv_worker_reply(reply_rx, "query")
    }

    pub fn query_row(&self, sql: &str) -> KnowledgeResult<RowData> {
        let worker = self.pick_worker()?;
        let (reply_tx, reply_rx) = mpsc::channel();
        worker.send(PostgresCommand::QueryRow {
            sql: sql.to_string(),
            reply: reply_tx,
        })?;
        recv_worker_reply(reply_rx, "query_row")
    }

    pub fn query_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<Vec<RowData>> {
        let worker = self.pick_worker()?;
        let (reply_tx, reply_rx) = mpsc::channel();
        worker.send(PostgresCommand::QueryFields {
            sql: sql.to_string(),
            params: params.to_vec(),
            reply: reply_tx,
        })?;
        recv_worker_reply(reply_rx, "query_fields")
    }

    pub fn query_named_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<RowData> {
        self.query_fields(sql, params)
            .map(|rows| rows.into_iter().next().unwrap_or_default())
    }
    fn pick_worker(&self) -> KnowledgeResult<&PostgresWorkerHandle> {
        if self.workers.is_empty() {
            return Err(KnowledgeReason::from_conf()
                .to_err()
                .with_detail("postgres worker pool is empty"));
        }
        let idx = self.next_worker.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        Ok(&self.workers[idx])
    }
}

impl Drop for PostgresProvider {
    fn drop(&mut self) {
        for worker in &self.workers {
            let _ = worker.send(PostgresCommand::Shutdown);
        }
        if let Ok(mut threads) = self.threads.lock() {
            for thread in threads.drain(..) {
                let _ = thread.join();
            }
        }
    }
}

fn spawn_worker(
    connection_uri: &str,
    worker_idx: usize,
) -> KnowledgeResult<(PostgresWorkerHandle, JoinHandle<()>)> {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let connection_uri = connection_uri.to_string();
    let builder = thread::Builder::new().name(format!("wp-kdb-pg-{worker_idx}"));
    let thread = builder
        .spawn(move || postgres_worker_loop(connection_uri, cmd_rx, ready_tx))
        .map_err(|err| {
            KnowledgeReason::from_conf()
                .to_err()
                .with_detail(format!("spawn postgres worker failed: {err}"))
        })?;

    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok((PostgresWorkerHandle { tx: cmd_tx }, thread)),
        Ok(Err(err)) => {
            let _ = thread.join();
            Err(err)
        }
        Err(err) => Err(KnowledgeReason::from_conf()
            .to_err()
            .with_detail(format!("postgres worker startup timed out: {err}"))),
    }
}

fn postgres_worker_loop(
    connection_uri: String,
    cmd_rx: mpsc::Receiver<PostgresCommand>,
    ready_tx: mpsc::Sender<KnowledgeResult<()>>,
) {
    let mut client = match Client::connect(&connection_uri, NoTls) {
        Ok(client) => client,
        Err(err) => {
            let _ = ready_tx.send(Err(KnowledgeReason::from_conf()
                .to_err()
                .with_detail(format!("create postgres client failed: {err}"))));
            return;
        }
    };

    if let Err(err) = client.simple_query("SELECT 1") {
        let _ = ready_tx.send(Err(validation_err("connection", err)));
        return;
    }
    let _ = ready_tx.send(Ok(()));

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            PostgresCommand::Query { sql, reply } => {
                let _ = reply.send(execute_query(&mut client, &sql));
            }
            PostgresCommand::QueryRow { sql, reply } => {
                let _ = reply.send(execute_query_row(&mut client, &sql));
            }
            PostgresCommand::QueryFields { sql, params, reply } => {
                let _ = reply.send(execute_query_fields(&mut client, &sql, &params));
            }
            PostgresCommand::Shutdown => break,
        }
    }
}

fn recv_worker_reply<T>(
    rx: mpsc::Receiver<KnowledgeResult<T>>,
    action: &str,
) -> KnowledgeResult<T> {
    rx.recv().map_err(|err| {
        KnowledgeReason::from_conf().to_err().with_detail(format!(
            "postgres worker reply failed during {action}: {err}"
        ))
    })?
}

fn execute_query(client: &mut Client, sql: &str) -> KnowledgeResult<Vec<RowData>> {
    let mut prepared_stmt = None;
    let col_names = metadata_cache_get_or_try_init(sql, || {
        let stmt = client.prepare(sql).map_err(|err| {
            KnowledgeReason::from_rule()
                .to_err()
                .with_detail(format!("postgres query prepare failed: {err}"))
        })?;
        let names = statement_col_names(&stmt);
        prepared_stmt = Some(stmt);
        Ok(Some(names))
    })?;
    let rows = if let Some(stmt) = prepared_stmt.as_ref() {
        client.query(stmt, &[])
    } else {
        client.query(sql, &[])
    }
    .map_err(|err| {
        KnowledgeReason::from_rule()
            .to_err()
            .with_detail(format!("postgres query failed: {err}"))
    })?;
    rows.iter().map(|row| map_row(row, &col_names)).collect()
}

fn execute_query_row(client: &mut Client, sql: &str) -> KnowledgeResult<RowData> {
    let mut prepared_stmt = None;
    let col_names = metadata_cache_get_or_try_init(sql, || {
        let stmt = client.prepare(sql).map_err(|err| {
            KnowledgeReason::from_rule()
                .to_err()
                .with_detail(format!("postgres query_row prepare failed: {err}"))
        })?;
        let names = statement_col_names(&stmt);
        prepared_stmt = Some(stmt);
        Ok(Some(names))
    })?;
    let rows = if let Some(stmt) = prepared_stmt.as_ref() {
        client.query(stmt, &[])
    } else {
        client.query(sql, &[])
    }
    .map_err(|err| {
        KnowledgeReason::from_rule()
            .to_err()
            .with_detail(format!("postgres query_row failed: {err}"))
    })?;
    if let Some(row) = rows.first() {
        map_row(row, &col_names)
    } else {
        Ok(Vec::new())
    }
}

fn execute_query_fields(
    client: &mut Client,
    sql: &str,
    params: &[DataField],
) -> KnowledgeResult<Vec<RowData>> {
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
    let col_names = metadata_cache_get_or_try_init(sql, || {
        let stmt = client.prepare(&rewritten_sql).map_err(|err| {
            KnowledgeReason::from_rule()
                .to_err()
                .with_detail(format!("postgres query_fields prepare failed: {err}"))
        })?;
        let names = statement_col_names(&stmt);
        prepared_stmt = Some(stmt);
        Ok(Some(names))
    })?;
    let rows = if let Some(stmt) = prepared_stmt.as_ref() {
        client.query(stmt, &bind_refs)
    } else {
        client.query(&rewritten_sql, &bind_refs)
    }
    .map_err(|err| {
        KnowledgeReason::from_rule()
            .to_err()
            .with_detail(format!("postgres query_fields failed: {err}"))
    })?;
    rows.iter().map(|row| map_row(row, &col_names)).collect()
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

fn validation_err(stage: &str, err: postgres::Error) -> wp_error::KnowledgeError {
    KnowledgeReason::from_conf().to_err().with_detail(format!(
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
    let mut out = String::with_capacity(sql.len());
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;

    while i < chars.len() {
        let ch = chars[i];
        if ch == '\'' && !in_double {
            in_single = !in_single;
            out.push(ch);
            i += 1;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            out.push(ch);
            i += 1;
            continue;
        }

        if ch == ':' && !in_single && !in_double {
            let start = i;
            i += 1;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let raw_name: String = chars[start..i].iter().collect();
            let field = by_name.get(&raw_name).ok_or_else(|| {
                KnowledgeReason::from_rule()
                    .to_err()
                    .with_detail(format!("postgres query missing param: {raw_name}"))
            })?;
            let placeholder_no = if let Some(idx) = assigned_numbers.get(&raw_name) {
                *idx
            } else {
                let idx = ordered.len() + 1;
                assigned_numbers.insert(raw_name.clone(), idx);
                ordered.push(*field);
                idx
            };
            out.push('$');
            out.push_str(&placeholder_no.to_string());
            continue;
        }

        out.push(ch);
        i += 1;
    }

    Ok((out, ordered))
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
        Type::BOOL => row
            .try_get::<usize, Option<bool>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_bool(name.to_string(), value))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))
            .pipe(Ok),
        Type::INT2 => row
            .try_get::<usize, Option<i16>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_digit(name.to_string(), i64::from(value)))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))
            .pipe(Ok),
        Type::INT4 => row
            .try_get::<usize, Option<i32>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_digit(name.to_string(), i64::from(value)))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))
            .pipe(Ok),
        Type::INT8 | Type::OID => row
            .try_get::<usize, Option<i64>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_digit(name.to_string(), value))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))
            .pipe(Ok),
        Type::FLOAT4 => row
            .try_get::<usize, Option<f32>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_float(name.to_string(), f64::from(value)))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))
            .pipe(Ok),
        Type::FLOAT8 => row
            .try_get::<usize, Option<f64>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_float(name.to_string(), value))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))
            .pipe(Ok),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => row
            .try_get::<usize, Option<String>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_chars(name.to_string(), value))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))
            .pipe(Ok),
        Type::JSON | Type::JSONB => row
            .try_get::<usize, Option<JsonValue>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_chars(name.to_string(), value.to_string()))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))
            .pipe(Ok),
        Type::TIMESTAMP => row
            .try_get::<usize, Option<NaiveDateTime>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_chars(name.to_string(), value.to_string()))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))
            .pipe(Ok),
        Type::TIMESTAMPTZ => row
            .try_get::<usize, Option<DateTime<Utc>>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_chars(name.to_string(), value.to_rfc3339()))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))
            .pipe(Ok),
        Type::DATE => row
            .try_get::<usize, Option<NaiveDate>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_chars(name.to_string(), value.to_string()))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))
            .pipe(Ok),
        Type::TIME => row
            .try_get::<usize, Option<NaiveTime>>(idx)
            .map_err(pg_decode_err)?
            .map(|value| DataField::from_chars(name.to_string(), value.to_string()))
            .unwrap_or_else(|| DataField::new(DataType::default(), name, Value::Null))
            .pipe(Ok),
        _ => Err(KnowledgeReason::from_rule().to_err().with_detail(format!(
            "postgres column type not supported yet: {} ({})",
            name,
            ty.name()
        ))),
    }
}

fn pg_decode_err(err: postgres::Error) -> wp_error::KnowledgeError {
    KnowledgeReason::from_rule()
        .to_err()
        .with_detail(format!("postgres decode failed: {err}"))
}

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}

impl<T> Pipe for T {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_sql_reuses_same_named_param() {
        let params = vec![DataField::from_digit(":id".to_string(), 7)];
        let (sql, ordered) =
            rewrite_sql("select * from demo where id=:id or parent_id=:id", &params).unwrap();
        assert_eq!(sql, "select * from demo where id=$1 or parent_id=$1");
        assert_eq!(ordered.len(), 1);
        assert_eq!(ordered[0].get_name(), ":id");
    }

    #[test]
    fn rewrite_sql_ignores_colons_inside_strings() {
        let params = vec![DataField::from_digit(":id".to_string(), 7)];
        let (sql, ordered) = rewrite_sql(
            "select ':id' as literal, id from demo where id=:id",
            &params,
        )
        .unwrap();
        assert_eq!(sql, "select ':id' as literal, id from demo where id=$1");
        assert_eq!(ordered.len(), 1);
    }
}
