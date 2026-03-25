use std::time::Duration;

use ::mysql::prelude::Queryable;
use ::mysql::{
    Opts, OptsBuilder, Pool, PoolConstraints, PoolOpts, PooledConn, Row, Value as MySqlValue,
};
use orion_error::{ToStructError, UvsFrom};
use wp_error::{KnowledgeReason, KnowledgeResult};
use wp_model_core::model::{DataField, DataType, Value};

use crate::mem::RowData;

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
    pool: Pool,
}

impl MySqlProvider {
    pub fn connect(config: &MySqlProviderConfig) -> KnowledgeResult<Self> {
        let opts = Opts::from_url(config.connection_uri()).map_err(|err| {
            KnowledgeReason::from_conf()
                .to_err()
                .with_detail(format!("parse mysql connection uri failed: {err}"))
        })?;
        let constraints =
            PoolConstraints::new(0, config.pool_size() as usize).ok_or_else(|| {
                KnowledgeReason::from_conf()
                    .to_err()
                    .with_detail("invalid mysql pool constraints")
            })?;
        let pool_opts = PoolOpts::default().with_constraints(constraints);
        let opts = OptsBuilder::from_opts(opts).pool_opts(pool_opts);
        let pool = Pool::new(Opts::from(opts)).map_err(|err| {
            KnowledgeReason::from_conf()
                .to_err()
                .with_detail(format!("create mysql pool failed: {err}"))
        })?;

        validate_startup(&pool)?;

        Ok(Self { pool })
    }

    pub fn query(&self, sql: &str) -> KnowledgeResult<Vec<RowData>> {
        let mut conn = self.pool_conn("query")?;
        let rows: Vec<Row> = conn.query(sql).map_err(|err| {
            KnowledgeReason::from_rule()
                .to_err()
                .with_detail(format!("mysql query failed: {err}"))
        })?;
        rows.into_iter().map(map_row).collect()
    }

    pub fn query_row(&self, sql: &str) -> KnowledgeResult<RowData> {
        let mut conn = self.pool_conn("query_row")?;
        let row: Option<Row> = conn.query_first(sql).map_err(|err| {
            KnowledgeReason::from_rule()
                .to_err()
                .with_detail(format!("mysql query_row failed: {err}"))
        })?;
        match row {
            Some(row) => map_row(row),
            None => Ok(Vec::new()),
        }
    }

    pub fn query_named_fields(&self, sql: &str, params: &[DataField]) -> KnowledgeResult<RowData> {
        let mut conn = self.pool_conn("query_named")?;
        let row: Option<Row> = conn
            .exec_first(sql, mysql_named_params(params))
            .map_err(|err| {
                KnowledgeReason::from_rule()
                    .to_err()
                    .with_detail(format!("mysql query_named failed: {err}"))
            })?;
        match row {
            Some(row) => map_row(row),
            None => Ok(Vec::new()),
        }
    }

    fn pool_conn(&self, action: &str) -> KnowledgeResult<PooledConn> {
        self.pool
            .try_get_conn(Duration::from_secs(5))
            .map_err(|err| {
                KnowledgeReason::from_conf().to_err().with_detail(format!(
                    "mysql {action} failed to acquire pooled connection: {err}"
                ))
            })
    }
}

fn validate_startup(pool: &Pool) -> KnowledgeResult<()> {
    let mut conn = pool.try_get_conn(Duration::from_secs(5)).map_err(|err| {
        KnowledgeReason::from_conf().to_err().with_detail(format!(
            "mysql startup validation failed to acquire pooled connection: {err}"
        ))
    })?;
    conn.query_drop("SELECT 1")
        .map_err(|err| validation_err("connection", err))?;
    Ok(())
}

fn validation_err(stage: &str, err: ::mysql::Error) -> wp_error::KnowledgeError {
    KnowledgeReason::from_conf().to_err().with_detail(format!(
        "mysql startup validation failed during {stage}: connection issue: {err}"
    ))
}

fn mysql_named_params(params: &[DataField]) -> Vec<(Vec<u8>, MySqlValue)> {
    params
        .iter()
        .map(|field| (normalize_param_name(field.get_name()), mysql_value(field)))
        .collect()
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

fn map_row(row: Row) -> KnowledgeResult<RowData> {
    let mut result = Vec::with_capacity(row.len());
    for (idx, column) in row.columns_ref().iter().enumerate() {
        let name = column.name_str().into_owned();
        let field = match row.as_ref(idx) {
            Some(value) => map_value(&name, value),
            None => DataField::new(DataType::default(), &name, Value::Null),
        };
        result.push(field);
    }
    Ok(result)
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
