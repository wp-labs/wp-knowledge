//! SQL 路由工具：从查询 SQL 中识别命名 provider 前缀并剥离。
//!
//! OML 查询如 `select country_name from geo.public.ip_geo_city where ip_num = :ip_num`
//! 中，首个表引用第一段 `geo` 指向已安装的命名 provider，`geo.` 前缀会被剥离后
//! 再交给 PostgreSQL（否则 PG 会把三段名当作 `catalog.schema.table` 并报跨库错误）。

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{OnceLock, RwLock};

use crate::runtime::runtime;

/// 路由 memo 容量上限：防 SQL 文本无界增长。
const ROUTE_MEMO_CAP: usize = 1024;

struct RouteMemo {
    epoch: u64,
    map: HashMap<u64, Option<(String, String)>>,
}

fn route_memo() -> &'static RwLock<RouteMemo> {
    static MEMO: OnceLock<RwLock<RouteMemo>> = OnceLock::new();
    MEMO.get_or_init(|| {
        RwLock::new(RouteMemo {
            epoch: 0,
            map: HashMap::new(),
        })
    })
}

fn sql_route_key(sql: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    sql.hash(&mut hasher);
    hasher.finish()
}

/// 从 SQL 中提取路由信息：若首个表引用第一段命中已安装的命名 provider，
/// 返回 `(provider_name, 剥离前缀后的 SQL)`；否则返回 `None`（走默认 provider）。
///
/// 结果按 `(sql hash, provider 注册表版本号)` 记忆化，热路径避免重复扫描；
/// provider 安装/清理时版本号递增使 memo 失效。
pub fn route_provider_sql(sql: &str) -> Option<(String, String)> {
    let epoch = runtime().named_provider_epoch();
    let key = sql_route_key(sql);
    {
        let memo = route_memo()
            .read()
            .expect("sql route memo read lock poisoned");
        if memo.epoch == epoch
            && let Some(routed) = memo.map.get(&key)
        {
            return routed.clone();
        }
    }
    let routed = compute_route(sql);
    let mut memo = route_memo()
        .write()
        .expect("sql route memo write lock poisoned");
    if memo.epoch != epoch {
        memo.epoch = epoch;
        memo.map.clear();
    }
    if memo.map.len() >= ROUTE_MEMO_CAP {
        memo.map.clear();
    }
    memo.map.insert(key, routed.clone());
    routed
}

fn compute_route(sql: &str) -> Option<(String, String)> {
    let table = first_table_name(sql)?;
    let name = table.split('.').next()?;
    if name.is_empty() || !runtime().provider_exists(name) {
        return None;
    }
    Some((name.to_string(), strip_provider_prefix(sql, name)))
}

fn is_ident_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

/// 查找关键词，跳过字符串字面量、反引号、方括号内的内容。
fn find_keyword(sql: &str, keyword: &[u8]) -> Option<usize> {
    let bytes = sql.as_bytes();
    let mut idx = 0usize;

    while idx < bytes.len() {
        match bytes[idx] {
            b'\'' | b'"' => {
                let quote = bytes[idx];
                idx += 1;
                while idx < bytes.len() {
                    if bytes[idx] == quote {
                        idx += 1;
                        if idx < bytes.len() && bytes[idx] == quote {
                            idx += 1;
                            continue;
                        }
                        break;
                    }
                    idx += 1;
                }
            }
            b'`' => {
                idx += 1;
                while idx < bytes.len() && bytes[idx] != b'`' {
                    idx += 1;
                }
                idx += usize::from(idx < bytes.len());
            }
            b'[' => {
                idx += 1;
                while idx < bytes.len() && bytes[idx] != b']' {
                    idx += 1;
                }
                idx += usize::from(idx < bytes.len());
            }
            b'-' if bytes.get(idx + 1) == Some(&b'-') => {
                // 行注释：跳过到行尾
                idx += 2;
                while idx < bytes.len() && bytes[idx] != b'\n' {
                    idx += 1;
                }
            }
            b'/' if bytes.get(idx + 1) == Some(&b'*') => {
                // 块注释：跳过到 `*/`
                idx += 2;
                while idx + 1 < bytes.len() && !(bytes[idx] == b'*' && bytes[idx + 1] == b'/') {
                    idx += 1;
                }
                if idx + 1 < bytes.len() {
                    idx += 2;
                } else {
                    idx = bytes.len();
                }
            }
            _ => {
                let end = idx + keyword.len();
                if end <= bytes.len()
                    && bytes[idx..end].eq_ignore_ascii_case(keyword)
                    && idx
                        .checked_sub(1)
                        .is_none_or(|prev| !is_ident_byte(bytes[prev]))
                    && bytes.get(end).is_none_or(|next| !is_ident_byte(*next))
                {
                    return Some(idx);
                }
                idx += 1;
            }
        }
    }

    None
}

/// 找到与 `open_pos` 处左括号匹配的右括号位置。
fn matching_paren(sql: &str, open_pos: usize) -> Option<usize> {
    let bytes = sql.as_bytes();
    let mut depth = 0usize;
    let mut idx = open_pos;

    while idx < bytes.len() {
        match bytes[idx] {
            b'\'' | b'"' => {
                let quote = bytes[idx];
                idx += 1;
                while idx < bytes.len() {
                    if bytes[idx] == quote {
                        idx += 1;
                        if idx < bytes.len() && bytes[idx] == quote {
                            idx += 1;
                            continue;
                        }
                        break;
                    }
                    idx += 1;
                }
            }
            b'`' => {
                idx += 1;
                while idx < bytes.len() && bytes[idx] != b'`' {
                    idx += 1;
                }
                idx += usize::from(idx < bytes.len());
            }
            b'[' => {
                idx += 1;
                while idx < bytes.len() && bytes[idx] != b']' {
                    idx += 1;
                }
                idx += usize::from(idx < bytes.len());
            }
            b'-' if bytes.get(idx + 1) == Some(&b'-') => {
                idx += 2;
                while idx < bytes.len() && bytes[idx] != b'\n' {
                    idx += 1;
                }
            }
            b'/' if bytes.get(idx + 1) == Some(&b'*') => {
                idx += 2;
                while idx + 1 < bytes.len() && !(bytes[idx] == b'*' && bytes[idx + 1] == b'/') {
                    idx += 1;
                }
                if idx + 1 < bytes.len() {
                    idx += 2;
                } else {
                    idx = bytes.len();
                }
            }
            b'(' => {
                depth += 1;
                idx += 1;
            }
            b')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(idx);
                }
                idx += 1;
            }
            _ => idx += 1,
        }
    }

    None
}

/// 返回 SQL 中首个 FROM 表引用（跳过字符串与注释，递归处理子查询）。
///
/// 供路由与表判定复用（wp-oml 的 `resolve_sql_route` 也使用本实现）。
pub fn first_table_name(sql: &str) -> Option<&str> {
    let from_pos = find_keyword(sql, b"from")?;
    let mut table_start = from_pos + b"from".len();
    let bytes = sql.as_bytes();
    while table_start < bytes.len() && bytes[table_start].is_ascii_whitespace() {
        table_start += 1;
    }

    if bytes.get(table_start) == Some(&b'(') {
        let close_pos = matching_paren(sql, table_start)?;
        return first_table_name(&sql[table_start + 1..close_pos]);
    }

    let table_end = sql[table_start..]
        .find(|c: char| c.is_ascii_whitespace() || matches!(c, ';' | ',' | ')'))
        .map_or(sql.len(), |end| table_start + end);
    let table = sql[table_start..table_end].trim();
    if table.is_empty() { None } else { Some(table) }
}

/// 引号感知地剥离 SQL 中所有 `<name>.` 前缀（仅当处于标识符起始位置，
/// 即前一个字符不是标识符或 `.`，且 `<name>.` 后紧跟标识符）。
///
/// 示例：`select geo.country_name from geo.public.t` → `select country_name from public.t`；
/// 字符串字面量 `'geo.x'` 与 `geography.x` 不受影响。
pub fn strip_provider_prefix(sql: &str, name: &str) -> String {
    if name.is_empty() {
        return sql.to_string();
    }
    let bytes = sql.as_bytes();
    let name_bytes = name.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        // 引号/注释块：原样拷贝，跳过内部匹配
        match b {
            b'\'' | b'"' | b'`' => {
                let quote = b;
                let start = i;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == quote {
                        i += 1;
                        if i < bytes.len() && bytes[i] == quote {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                out.push_str(&sql[start..i]);
                continue;
            }
            b'[' => {
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != b']' {
                    i += 1;
                }
                i += usize::from(i < bytes.len());
                out.push_str(&sql[start..i]);
                continue;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                let start = i;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                out.push_str(&sql[start..i]);
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let start = i;
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                if i + 1 < bytes.len() {
                    i += 2;
                } else {
                    i = bytes.len();
                }
                out.push_str(&sql[start..i]);
                continue;
            }
            _ => {}
        }
        // 标识符起始边界：`name` 后紧跟 `.`，前一个字符不是标识符或 `.`。
        let prev = i.checked_sub(1).map(|p| bytes[p]);
        let is_boundary = prev.is_none_or(|p| !is_ident_byte(p) && p != b'.');
        if is_boundary
            && i + name_bytes.len() < bytes.len()
            && &bytes[i..i + name_bytes.len()] == name_bytes
            && bytes[i + name_bytes.len()] == b'.'
            && bytes
                .get(i + name_bytes.len() + 1)
                .is_some_and(|next| is_ident_byte(*next) || matches!(*next, b'"' | b'`' | b'['))
        {
            i += name_bytes.len() + 1;
            continue;
        }
        out.push(b as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::ProviderKind;
    use crate::runtime::{DatasourceId, runtime, runtime_test_guard};

    fn install_test_provider(name: &str) {
        use crate::error::KnowledgeResult;
        use crate::mem::RowData;
        use crate::runtime::ProviderExecutor;
        use async_trait::async_trait;
        use std::sync::Arc;
        use wp_model_core::model::{DataField, DataType, Value};

        struct TestProvider;

        #[async_trait]
        impl ProviderExecutor for TestProvider {
            fn query(&self, _sql: &str) -> KnowledgeResult<Vec<RowData>> {
                Ok(vec![vec![DataField::new(
                    DataType::default(),
                    "v",
                    Value::Null,
                )]])
            }
            fn query_fields(
                &self,
                _sql: &str,
                _params: &[DataField],
            ) -> KnowledgeResult<Vec<RowData>> {
                self.query("")
            }
            fn query_row(&self, _sql: &str) -> KnowledgeResult<RowData> {
                Ok(vec![DataField::new(DataType::default(), "v", Value::Null)])
            }
            fn query_named_fields(
                &self,
                _sql: &str,
                _params: &[DataField],
            ) -> KnowledgeResult<RowData> {
                self.query_row("")
            }
        }

        runtime()
            .install_provider_named(
                name,
                ProviderKind::Postgres,
                DatasourceId::from_seed(ProviderKind::Postgres, name),
                |_generation| Ok(Arc::new(TestProvider)),
                false,
            )
            .expect("install named provider");
    }

    #[test]
    fn route_provider_sql_matches_installed_provider() {
        let _guard = runtime_test_guard().lock().expect("guard");
        install_test_provider("geo");

        let (name, stripped) = route_provider_sql(
            "select country_name from geo.public.ip_geo_city where ip_num = :ip",
        )
        .expect("route to geo");
        assert_eq!(name, "geo");
        assert_eq!(
            stripped,
            "select country_name from public.ip_geo_city where ip_num = :ip"
        );
    }

    #[test]
    fn route_provider_sql_falls_back_for_unknown_or_unqualified() {
        let _guard = runtime_test_guard().lock().expect("guard");
        install_test_provider("geo");

        // 未安装的 provider 名 → 不路由
        assert!(route_provider_sql("select a from nope.public.t").is_none());
        // 无前缀 → 不路由
        assert!(route_provider_sql("select a from ip_geo_city").is_none());
    }

    #[test]
    fn strip_keeps_string_literals_and_mid_dotted_idents() {
        let sql = "select geo.country_name from geo.public.t where name = 'geo.x'";
        let out = strip_provider_prefix(sql, "geo");
        assert_eq!(
            out,
            "select country_name from public.t where name = 'geo.x'"
        );

        // 非起始位置的 `geo.` 不剥离
        let sql2 = "select a.geo.country from geo.public.t";
        let out2 = strip_provider_prefix(sql2, "geo");
        assert_eq!(out2, "select a.geo.country from public.t");

        // 不是 `geo.` 前缀的标识符不受影响
        let sql3 = "select geography.x from geo.public.t";
        let out3 = strip_provider_prefix(sql3, "geo");
        assert_eq!(out3, "select geography.x from public.t");
    }

    #[test]
    fn strip_handles_distinct_qualifiers() {
        let sql = "select group_concat(distinct geo.asset_type) from geo.asset_enrichment";
        let out = strip_provider_prefix(sql, "geo");
        assert_eq!(
            out,
            "select group_concat(distinct asset_type) from asset_enrichment"
        );
    }

    #[test]
    fn route_provider_sql_handles_subquery() {
        let _guard = runtime_test_guard().lock().expect("guard");
        install_test_provider("geo");

        let sql = "select a from (select a from geo.public.t where x = 1) sub";
        let (name, stripped) = route_provider_sql(sql).expect("route subquery to geo");
        assert_eq!(name, "geo");
        assert_eq!(
            stripped,
            "select a from (select a from public.t where x = 1) sub"
        );
    }

    #[test]
    fn route_provider_sql_prefix_requires_installed_provider() {
        let _guard = runtime_test_guard().lock().expect("guard");
        // 未安装的 provider 名（无任何测试安装），即使带前缀也不路由（SQL 原样落回默认 provider）
        assert!(route_provider_sql("select a from ghost.public.t").is_none());
    }

    #[test]
    fn route_provider_sql_ignores_from_inside_comment() {
        let _guard = runtime_test_guard().lock().expect("guard");
        install_test_provider("geo");

        // 块注释内的 `from ghost.x` 不应干扰表引用解析
        let sql = "select 1 /* from ghost.x */ from geo.public.t";
        let (name, stripped) = route_provider_sql(sql).expect("route to geo");
        assert_eq!(name, "geo");
        assert_eq!(stripped, "select 1 /* from ghost.x */ from public.t");

        // 行注释同理
        let sql2 = "select 1 -- from ghost.x\nfrom geo.public.t";
        let (name, stripped2) = route_provider_sql(sql2).expect("route to geo");
        assert_eq!(name, "geo");
        assert_eq!(stripped2, "select 1 -- from ghost.x\nfrom public.t");
    }

    #[test]
    fn strip_skips_comments() {
        let sql = "select geo.a -- geo.b\nfrom geo.public.t /* geo.c */";
        let out = strip_provider_prefix(sql, "geo");
        assert_eq!(out, "select a -- geo.b\nfrom public.t /* geo.c */");
    }

    #[test]
    fn strip_keeps_backtick_quoted_identifiers() {
        let sql = "select `geo.col` from geo.public.t";
        let out = strip_provider_prefix(sql, "geo");
        assert_eq!(out, "select `geo.col` from public.t");
    }

    #[test]
    fn strip_handles_provider_name_with_underscore_and_digits() {
        let sql = "select x from geo_db_v1.public.t where geo_db_v1.k = 1";
        let out = strip_provider_prefix(sql, "geo_db_v1");
        assert_eq!(out, "select x from public.t where k = 1");
    }

    #[test]
    fn strip_does_not_touch_name_inside_longer_identifier() {
        // `geox.`（name 后不是点）与 `xgeo.`（name 不在标识符起始）都不剥离
        let sql = "select geox.a, xgeo.b from geo.public.t";
        let out = strip_provider_prefix(sql, "geo");
        assert_eq!(out, "select geox.a, xgeo.b from public.t");
    }

    #[test]
    fn strip_supports_quoted_table_names() {
        // 引号表名：`geo."t"` / `geo.\`t\`` / `geo.[t]` 也剥离前缀
        let out = strip_provider_prefix("select x from geo.\"public\".\"t\"", "geo");
        assert_eq!(out, "select x from \"public\".\"t\"");

        let out = strip_provider_prefix("select x from geo.`public`.`t`", "geo");
        assert_eq!(out, "select x from `public`.`t`");

        let out = strip_provider_prefix("select x from geo.[public].[t]", "geo");
        assert_eq!(out, "select x from [public].[t]");
    }
}
