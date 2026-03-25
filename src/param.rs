use orion_error::{ToStructError, UvsFrom};
use rusqlite::ToSql;
use rusqlite::types::{ToSqlOutput, Value as SqlValue, ValueRef};
use wp_error::{KnowledgeReason, KnowledgeResult};
use wp_model_core::model::{DataField, DataType, Value};

fn normalize_param_name(name: &str) -> String {
    if name.starts_with(':') {
        name.to_string()
    } else {
        format!(":{}", name)
    }
}

pub fn named_params_to_fields(params: &[(&str, &dyn ToSql)]) -> KnowledgeResult<Vec<DataField>> {
    params
        .iter()
        .map(|(name, value)| {
            let output = value.to_sql().map_err(|err| {
                KnowledgeReason::from_rule()
                    .to_err()
                    .with_detail(format!("sql param encode failed: {err}"))
            })?;
            data_field_from_sql_output(normalize_param_name(name), output)
        })
        .collect()
}

fn data_field_from_sql_output(name: String, output: ToSqlOutput<'_>) -> KnowledgeResult<DataField> {
    match output {
        ToSqlOutput::Borrowed(value) => data_field_from_value_ref(name, value),
        ToSqlOutput::Owned(value) => Ok(data_field_from_value(name, value)),
        _ => Ok(DataField::new(DataType::default(), name, Value::Null)),
    }
}

fn data_field_from_value_ref(name: String, value: ValueRef<'_>) -> KnowledgeResult<DataField> {
    Ok(match value {
        ValueRef::Null => DataField::new(DataType::default(), name, Value::Null),
        ValueRef::Integer(value) => DataField::from_digit(name, value),
        ValueRef::Real(value) => DataField::from_float(name, value),
        ValueRef::Text(value) => DataField::from_chars(
            name,
            String::from_utf8(value.to_vec()).map_err(|err| {
                KnowledgeReason::from_rule()
                    .to_err()
                    .with_detail(format!("sql text param utf8 decode failed: {err}"))
            })?,
        ),
        ValueRef::Blob(value) => {
            DataField::from_chars(name, String::from_utf8_lossy(value).to_string())
        }
    })
}

fn data_field_from_value(name: String, value: SqlValue) -> DataField {
    match value {
        SqlValue::Null => DataField::new(DataType::default(), name, Value::Null),
        SqlValue::Integer(value) => DataField::from_digit(name, value),
        SqlValue::Real(value) => DataField::from_float(name, value),
        SqlValue::Text(value) => DataField::from_chars(name, value),
        SqlValue::Blob(value) => {
            DataField::from_chars(name, String::from_utf8_lossy(&value).to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_sqlite_named_params() {
        let params = [("id", &7_i64 as &dyn ToSql)];
        let fields = named_params_to_fields(&params).expect("convert params");
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].get_name(), ":id");
    }
}
