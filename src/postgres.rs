use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use orion_error::{ToStructError, UvsFrom};
use postgres::types::{ToSql, Type};
use postgres::{Client, NoTls, Row};
use serde_json::Value as JsonValue;
use wp_error::{KnowledgeReason, KnowledgeResult};
use wp_model_core::model::{DataField, DataType, Value};

use crate::mem::RowData;

#[derive(Debug, Clone)]
pub struct PostgresProviderConfig {
    connection_uri: String,
}

impl PostgresProviderConfig {
    pub fn new(connection_uri: impl Into<String>) -> Self {
        Self {
            connection_uri: connection_uri.into(),
        }
    }

    pub fn connection_uri(&self) -> &str {
        &self.connection_uri
    }
}

pub struct PostgresProvider {
    client: Mutex<Client>,
}

impl PostgresProvider {
    pub fn connect(config: &PostgresProviderConfig) -> KnowledgeResult<Self> {
        let client = Client::connect(config.connection_uri(), NoTls).map_err(|err| {
            KnowledgeReason::from_conf()
                .to_err()
                .with_detail(format!("connect postgres failed: {err}"))
        })?;
        Ok(Self {
            client: Mutex::new(client),
        })
    }

    pub fn query(&self, sql: &str) -> KnowledgeResult<Vec<RowData>> {
        let mut client = self.client.lock().expect("postgres client lock poisoned");
        let rows = client.query(sql, &[]).map_err(|err| {
            KnowledgeReason::from_rule()
                .to_err()
                .with_detail(format!("postgres query failed: {err}"))
        })?;
        rows.iter().map(map_row).collect()
    }

    pub fn query_row(&self, sql: &str) -> KnowledgeResult<RowData> {
        let mut client = self.client.lock().expect("postgres client lock poisoned");
        let rows = client.query(sql, &[]).map_err(|err| {
            KnowledgeReason::from_rule()
                .to_err()
                .with_detail(format!("postgres query_row failed: {err}"))
        })?;
        if let Some(row) = rows.first() {
            map_row(row)
        } else {
            Ok(Vec::new())
        }
    }

    pub fn query_named_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<RowData> {
        let (rewritten_sql, ordered_params) = rewrite_sql(sql, params)?;
        let bind_values: Vec<PostgresBindValue> =
            ordered_params.iter().map(|field| PostgresBindValue::from(*field)).collect();
        let bind_refs: Vec<&(dyn ToSql + Sync)> =
            bind_values.iter().map(PostgresBindValue::as_tosql).collect();

        let mut client = self.client.lock().expect("postgres client lock poisoned");
        let rows = client.query(&rewritten_sql, &bind_refs).map_err(|err| {
            KnowledgeReason::from_rule()
                .to_err()
                .with_detail(format!("postgres query_named failed: {err}"))
        })?;
        if let Some(row) = rows.first() {
            map_row(row)
        } else {
            Ok(Vec::new())
        }
    }

    pub fn query_cipher(&self, table: &str) -> KnowledgeResult<Vec<String>> {
        let sql = format!("select value from {}", table);
        let mut client = self.client.lock().expect("postgres client lock poisoned");
        let rows = client.query(&sql, &[]).map_err(|err| {
            KnowledgeReason::from_rule()
                .to_err()
                .with_detail(format!("postgres query_cipher failed: {err}"))
        })?;

        let mut values = Vec::with_capacity(rows.len());
        for row in rows {
            let value: Option<String> = row.try_get(0).map_err(|err| {
                KnowledgeReason::from_rule()
                    .to_err()
                    .with_detail(format!("postgres cipher value decode failed: {err}"))
            })?;
            if let Some(value) = value {
                values.push(value);
            }
        }
        Ok(values)
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

fn map_row(row: &Row) -> KnowledgeResult<RowData> {
    let mut out = Vec::with_capacity(row.len());
    for (idx, col) in row.columns().iter().enumerate() {
        out.push(map_value(row, idx, col.name(), col.type_())?);
    }
    Ok(out)
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
        _ => Err(KnowledgeReason::from_rule()
            .to_err()
            .with_detail(format!(
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
        let (sql, ordered) =
            rewrite_sql("select ':id' as literal, id from demo where id=:id", &params).unwrap();
        assert_eq!(sql, "select ':id' as literal, id from demo where id=$1");
        assert_eq!(ordered.len(), 1);
    }
}
