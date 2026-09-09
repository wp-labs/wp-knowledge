//! VEL —— 变量求值语言（**V**ariable **E**valuation **L**anguage）。
//!
//! KnowDB 定期刷新（[`crate::refresh`]）的 SQL 供给可携带一小段 VEL 代码：
//! 每行一个赋值 `$name = 表达式`，刷新循环每次执行前按 knowdb 自身时钟求值，
//! 再用结果替换 SQL 模板里的 `$name` 占位符。语义完全归宿主配置；本模块只提供
//! 微型语法 + 内建函数表（无控制流，错误即配置错误，boot/首 tick 即暴露）。
//!
//! ## 语法
//!
//! ```text
//! code := 行*                       # 行 = 赋值 | 空行 | 注释（整行或行尾 '#'）
//! 赋值 := '$' 名 '=' 表达式
//! 表达式 := 字符串字面量 | 函数调用   # 表达式后只允许行尾 # 注释
//! 名    := [A-Za-z_][A-Za-z0-9_]*     # 重复定义 → 配置错误
//! ```
//!
//! - 字符串字面量：双引号包裹（如 `$max_age = "30 days"`），值原样透传（含 `#`）；
//! - 内建函数（参数为**秒**，标签 prefix 默认 `"p"`）：
//!
//! | 函数 | 值 |
//! |---|---|
//! | `phase_now(period_s, bucket_s[, prefix])` | 当前相位格标签 `prefix + fold(now)`，`fold(t) = (t mod period) div bucket` |
//! | `phase_next(period_s, bucket_s[, prefix])` | 下一相位格标签 `fold(now + bucket)`（周期末自动回绕首格） |
//!
//! 示例（demo PG 供给：基线只取当前/下一相位格在保留期内的收盘）：
//!
//! ```text
//! $max_age = "30 days"            # 保留期（写死）
//! $cur  = phase_now(240, 15)      # 当前相位格
//! $next = phase_next(240, 15)
//! ```
//!
//! 空代码 = 静态 SQL 直接执行。求值时钟见 [`current_wall_nanos`]。

use std::collections::HashSet;

use crate::error::{KnowReason, KnowledgeResult};
use orion_error::conversion::ToStructError;

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// VEL 赋值（解析产物）。
#[derive(Debug, Clone, PartialEq)]
pub enum VarDef {
    /// 字符串字面量透传（如 `$max_age = "2 hours"`）。
    Literal { name: String, value: String },
    /// 相位周期格（`cur`/`next` 内建）：`prefix + fold(now + offset_slots*bucket)`。
    PhaseBucket {
        name: String,
        period_s: u64,
        bucket_s: u64,
        offset_slots: i64,
        prefix: String,
    },
}

/// 解析 VEL 代码 → 有序赋值列表。
pub fn parse(code: &str) -> KnowledgeResult<Vec<VarDef>> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (idx, raw) in code.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((lhs, rhs)) = line.split_once('=') else {
            return err_at(idx, format!("缺 '=' 的赋值行: {line}"));
        };
        let name = lhs.trim();
        let valid = name.starts_with('$')
            && name.len() > 1
            && is_name_start(name[1..].chars().next().unwrap_or('_'))
            && name[1..].chars().all(is_name_char);
        if !valid {
            return err_at(
                idx,
                format!("左侧应为 $name（[A-Za-z_][A-Za-z0-9_]*），实际 {name}"),
            );
        }
        let key = name[1..].to_string();
        if !seen.insert(key.clone()) {
            return err_at(idx, format!("变量重复定义: {name}"));
        }
        let expr = rhs.trim();
        let var = parse_expr(&key, expr).map_err(|e| vel_err(format!("第{}行: {}", idx + 1, e)))?;
        out.push(var);
    }
    Ok(out)
}

fn parse_expr(name: &str, expr: &str) -> Result<VarDef, String> {
    let expr = expr.trim();
    // 字符串字面量："..."（值可含 #；右引号后只允许行尾注释）
    if let Some(after_open) = expr.strip_prefix('"') {
        let Some(q) = after_open.find('"') else {
            return Err(format!("字符串字面量未闭合: {expr}"));
        };
        validate_tail(&after_open[q + 1..], expr)?;
        return Ok(VarDef::Literal {
            name: name.to_string(),
            value: after_open[..q].to_string(),
        });
    }
    // 函数调用：name(a, b, c)（右括号后可带行尾注释）
    let Some(open) = expr.find('(') else {
        return Err(format!(
            "不支持的表达式: {expr}（支持 字符串字面量 / VEL 内建函数）"
        ));
    };
    let Some(close) = expr.rfind(')') else {
        return Err(format!("函数调用缺右括号: {expr}"));
    };
    if close < open {
        return Err(format!("函数调用缺右括号: {expr}"));
    }
    validate_tail(&expr[close + 1..], expr)?;
    let core = &expr[..=close];
    let fname = core[..open].trim();
    let args_raw = core[open + 1..close].trim();
    let args: Vec<&str> = if args_raw.is_empty() {
        Vec::new()
    } else {
        args_raw.split(',').map(|a| a.trim()).collect()
    };
    let num = |a: &str| -> Result<u64, String> {
        a.parse::<u64>()
            .map_err(|_| format!("{fname}() 参数应为正整数秒，实际 {a:?}"))
    };
    match (fname, args.len()) {
        ("phase_now", 2..=3) => Ok(VarDef::PhaseBucket {
            name: name.to_string(),
            period_s: num(args[0])?,
            bucket_s: num(args[1])?,
            offset_slots: 0,
            prefix: prefix_arg(args.get(2).copied())?,
        }),
        ("phase_next", 2..=3) => Ok(VarDef::PhaseBucket {
            name: name.to_string(),
            period_s: num(args[0])?,
            bucket_s: num(args[1])?,
            offset_slots: 1,
            prefix: prefix_arg(args.get(2).copied())?,
        }),
        ("phase_now" | "phase_next", n) => Err(format!(
            "{fname}() 需 2~3 参数 (period_s, bucket_s[, prefix])，实际 {n}"
        )),
        _ => Err(format!("未知 VEL 函数: {fname}")),
    }
}

/// 表达式（右引号/右括号）之后只允许空或行尾 `#` 注释。
fn validate_tail(tail: &str, whole: &str) -> Result<(), String> {
    let t = tail.trim();
    if t.is_empty() || t.starts_with('#') {
        Ok(())
    } else {
        Err(format!("表达式后只允许 # 注释，实际尾缀 {t:?}（{whole}）"))
    }
}

fn prefix_arg(arg: Option<&str>) -> Result<String, String> {
    match arg {
        None => Ok("p".to_string()),
        Some(a) if a.starts_with('"') && a.ends_with('"') && a.len() >= 2 => {
            Ok(a[1..a.len() - 1].to_string())
        }
        Some(a) => Err(format!("prefix 应为字符串字面量，实际 {a:?}")),
    }
}

fn err_at(idx: usize, msg: String) -> KnowledgeResult<Vec<VarDef>> {
    Err(vel_err(format!("第{}行: {}", idx + 1, msg)))
}

fn vel_err(msg: String) -> crate::error::KnowledgeError {
    KnowReason::from_res()
        .to_err()
        .with_detail(format!("VEL: {msg}"))
}

/// 在给定时刻求值全部变量（`(name, value)`；按代码行序）。
pub fn eval(code: &str, now_ns: u64) -> KnowledgeResult<Vec<(String, String)>> {
    let defs = parse(code)?;
    let mut out = Vec::with_capacity(defs.len());
    for d in &defs {
        match d {
            VarDef::Literal { name, value } => out.push((name.clone(), value.clone())),
            VarDef::PhaseBucket {
                name,
                period_s,
                bucket_s,
                offset_slots,
                prefix,
            } => {
                let period_ns = period_s.saturating_mul(1_000_000_000);
                let bucket_ns = bucket_s.saturating_mul(1_000_000_000);
                if period_ns == 0 || bucket_ns == 0 || bucket_ns > period_ns {
                    return Err(vel_err(format!(
                        "${name}: 相位参数非法（须 0<桶≤周期）: period={period_s} bucket={bucket_s}"
                    )));
                }
                let t = if *offset_slots >= 0 {
                    now_ns.saturating_add((*offset_slots as u64).saturating_mul(bucket_ns))
                } else {
                    now_ns.saturating_sub((-(*offset_slots) as u64).saturating_mul(bucket_ns))
                };
                let idx = (t % period_ns) / bucket_ns;
                out.push((name.clone(), format!("{prefix}{idx}")));
            }
        }
    }
    Ok(out)
}

/// 把 `$name` 占位符替换为对应值。
///
/// **标识符感知**：只匹配完整 `$` + 变量名（名 = `[A-Za-z_][A-Za-z0-9_]*`），
/// 不做子串替换——`$cur` 不会误伤 `$cur2`/`$cur_x`；未知 `$...` 原样保留
/// （值不应含 `$`）。
pub fn resolve_vars(sql: &str, vars: &[(String, String)]) -> String {
    if vars.is_empty() {
        return sql.to_string();
    }
    let map: std::collections::HashMap<&str, &str> =
        vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut out = String::with_capacity(sql.len());
    let mut rest = sql;
    while let Some(pos) = rest.find('$') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos + 1..]; // 消费 '$'
        let Some(first) = rest.chars().next() else {
            out.push('$');
            break;
        };
        if !is_name_start(first) {
            out.push('$'); // 非占位符的裸 '$'：原样保留，继续
            continue;
        }
        // 读取最长标识符并整体匹配
        let mut end = 0usize;
        for (idx, c) in rest.char_indices() {
            if !is_name_char(c) {
                break;
            }
            end = idx + c.len_utf8();
        }
        let ident = &rest[..end];
        match map.get(ident) {
            Some(value) => {
                out.push_str(value);
                rest = &rest[end..];
            }
            None => {
                out.push('$');
                out.push_str(ident);
                rest = &rest[end..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 求值 VEL 代码并渲染 SQL 模板。boot 装载可复用本函数保证与刷新同源。
pub fn render(sql: &str, code: &str, now_ns: u64) -> KnowledgeResult<String> {
    if code.trim().is_empty() {
        return Ok(sql.to_string());
    }
    let vars = eval(code, now_ns)?;
    Ok(resolve_vars(sql, &vars))
}

/// 当前墙钟 epoch 纳秒（VEL 求值的时钟来源）。
pub fn current_wall_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns(s: u64) -> u64 {
        s.saturating_mul(1_000_000_000)
    }

    #[test]
    fn eval_phase_functions_and_literals() {
        // period=240s/bucket=15s（N=16）：120s → 桶 8；下一格 135s → 桶 9。
        let code = r#"
# 注释 + 空行应忽略
$max_age = "2 hours"
$cur  = phase_now(240, 15)
$next = phase_next(240, 15)
"#;
        assert_eq!(
            eval(code, ns(120)).unwrap(),
            vec![
                ("max_age".to_string(), "2 hours".to_string()),
                ("cur".to_string(), "p8".to_string()),
                ("next".to_string(), "p9".to_string()),
            ]
        );
        // 周期末回绕：225s 末格 cur=p15；+1 格 240s → p0。
        let kv = eval(code, ns(225)).unwrap();
        let map: std::collections::HashMap<&str, &str> =
            kv.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(map["cur"], "p15");
        assert_eq!(map["next"], "p0");
        // 跨周期同相位复现：360s（120+240）→ 仍桶 8。
        let kv = eval(code, ns(360)).unwrap();
        assert_eq!(kv[1], ("cur".to_string(), "p8".to_string()));
    }

    #[test]
    fn custom_prefix_and_errors() {
        // 自定义前缀（可选第三参）
        let kv = eval("$b = phase_now(240, 15, \"slot\")", ns(120)).unwrap();
        assert_eq!(kv, vec![("b".to_string(), "slot8".to_string())]);
        // 未知函数 / 缺 = / 空代码 / 纯注释
        assert!(eval("$x = foo(1)", ns(0)).is_err(), "未知函数应报错");
        assert!(eval("no_assign", ns(0)).is_err(), "缺 = 应报错");
        assert!(eval("", ns(0)).unwrap().is_empty());
        assert!(eval("# 纯注释", ns(0)).unwrap().is_empty());
        // 非法相位参数 → 求值报错
        assert!(eval("$x = phase_now(15, 240)", ns(0)).is_err());
    }

    #[test]
    fn render_substitutes_and_empty_passes_through() {
        let code = "$cur = phase_now(240, 15)\n$max_age = \"2 hours\"";
        let sql = "SELECT * FROM t WHERE phase_bucket = '$cur' AND win_start >= now() - interval '$max_age'";
        assert_eq!(
            render(sql, code, ns(120)).unwrap(),
            "SELECT * FROM t WHERE phase_bucket = 'p8' AND win_start >= now() - interval '2 hours'"
        );
        // 空代码 = 原样返回
        assert_eq!(render(sql, "", ns(120)).unwrap(), sql);
        assert_eq!(render(sql, "  \n# note\n", ns(120)).unwrap(), sql);
    }

    #[test]
    fn resolve_replaces_and_keeps_unknown() {
        let vars = vec![
            ("cur".to_string(), "p7".to_string()),
            ("next".to_string(), "p8".to_string()),
        ];
        assert_eq!(
            resolve_vars("WHERE k IN ('$cur','$next') AND z='$ghost'", &vars),
            "WHERE k IN ('p7','p8') AND z='$ghost'"
        );
        assert_eq!(resolve_vars("WHERE 1", &[]), "WHERE 1");
    }

    #[test]
    fn resolve_vars_is_identifier_aware_not_substring() {
        // 变量名互为前缀时不得误伤：$cur 只替换完整标识符。
        let vars = vec![
            ("cur".to_string(), "p7".to_string()),
            ("cur2".to_string(), "x".to_string()),
            ("c".to_string(), "y".to_string()),
        ];
        let sql = "IN ('$cur','$cur2','$cur_x','$c','$c2','pair:$cur$cur') AND '$9' AND '$中文'";
        assert_eq!(
            resolve_vars(sql, &vars),
            "IN ('p7','x','$cur_x','y','$c2','pair:p7p7') AND '$9' AND '$中文'"
        );
        // 未知/裸 $ 保持原样
        assert_eq!(resolve_vars("$ghost", &vars), "$ghost");
        let vars2 = vec![("cur".to_string(), "p1".to_string())];
        assert_eq!(resolve_vars("$$cur", &vars2), "$p1");
        assert_eq!(resolve_vars("$cur$", &vars2), "p1$");
        assert_eq!(resolve_vars("$中文$cur", &vars2), "$中文p1");
    }

    #[test]
    fn parse_rejects_duplicate_and_invalid_names() {
        // 重复定义报错（静默后者覆盖是配置错误）
        assert!(parse("$a = \"1\"\n$a = \"2\"").is_err());
        // 数字开头的名字 / 非法字符拒绝
        assert!(parse("$1x = \"v\"").is_err());
        assert!(parse("$x-y = \"v\"").is_err());
        assert!(parse("x = \"v\"").is_err());
        // 合法名字：字母/下划线开头，可含数字
        assert!(parse("$_a = \"1\"\n$a_1 = \"2\"").is_ok());
    }

    #[test]
    fn inline_comments_supported_and_trailing_garbage_rejected() {
        let code = r#"
$max_age = "30 days"            # 保留期
$cur  = phase_now(240, 15)      # 当前格
$hint = "a # not comment"       # 引号内 # 是值
"#;
        let kv = eval(code, ns(120)).unwrap();
        let map: std::collections::HashMap<&str, &str> =
            kv.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(map["max_age"], "30 days");
        assert_eq!(map["cur"], "p8");
        assert_eq!(map["hint"], "a # not comment");
        // 表达式后的非注释尾缀 → 报错（而不是静默吞掉）
        assert!(parse("$a = \"v\" junk").is_err());
        assert!(parse("$a = phase_now(240, 15) junk").is_err());
        assert!(parse("$a = phase_now(240, 15))").is_err());
    }

    #[test]
    fn single_slot_period_equals_bucket_stays_stable() {
        // period == bucket：单格退化——任意时刻同一标签 p0。
        let code = "$cur = phase_now(60, 60)";
        for t in [0u64, 1, 59, 60, 7_000_000_000_000_000] {
            assert_eq!(
                eval(code, t).unwrap(),
                vec![("cur".to_string(), "p0".to_string())]
            );
        }
    }

    #[test]
    fn eval_is_deterministic_over_now() {
        // 同代码不同 now：结果只随相位位置变（确定性折桶，无隐藏状态）。
        let code = "$cur = phase_now(240, 15)";
        let a = eval(code, ns(120)).unwrap();
        let b = eval(code, ns(120)).unwrap();
        assert_eq!(a, b);
        let c = eval(code, ns(121)).unwrap();
        assert_eq!(c, vec![("cur".to_string(), "p8".to_string())], "同格内不变");
        let d = eval(code, ns(135)).unwrap();
        assert_eq!(d, vec![("cur".to_string(), "p9".to_string())], "跨格推进");
    }
}
