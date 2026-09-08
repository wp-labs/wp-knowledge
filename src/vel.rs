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
//! code := 行*                       # 行 = 赋值 | 空行 | # 注释
//! 赋值 := '$' 名 '=' 表达式
//! 表达式 := 字符串字面量 | 函数调用
//! ```
//!
//! - 字符串字面量：双引号包裹（如 `$max_age = "2 hours"`），值原样透传；
//! - 内建函数（参数为**秒**，标签 prefix 默认 `"p"`）：
//!
//! | 函数 | 值 |
//! |---|---|
//! | `cur_phase_bucket(period_s, bucket_s[, prefix])` | 当前相位格标签 `prefix + fold(now)`，`fold(t) = (t mod period) div bucket` |
//! | `next_phase_bucket(period_s, bucket_s[, prefix])` | 下一相位格标签 `fold(now + bucket)`（周期末自动回绕首格） |
//!
//! 示例（demo PG 供给：基线只取当前/下一相位格在保留期内的收盘）：
//!
//! ```text
//! $max_age = "2 hours"
//! $cur  = cur_phase_bucket(240, 15)
//! $next = next_phase_bucket(240, 15)
//! ```
//!
//! 空代码 = 静态 SQL 直接执行。求值时钟见 [`current_wall_nanos`]。

use crate::error::{KnowReason, KnowledgeResult};
use orion_error::conversion::ToStructError;

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
    for (idx, raw) in code.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((lhs, rhs)) = line.split_once('=') else {
            return err_at(idx, format!("缺 '=' 的赋值行: {line}"));
        };
        let name = lhs.trim();
        if !(name.starts_with('$')
            && name.len() > 1
            && name[1..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_'))
        {
            return err_at(idx, format!("左侧应为 $name，实际 {name}"));
        }
        let expr = rhs.trim();
        let var =
            parse_expr(&name[1..], expr).map_err(|e| vel_err(format!("第{}行: {}", idx + 1, e)))?;
        out.push(var);
    }
    Ok(out)
}

fn parse_expr(name: &str, expr: &str) -> Result<VarDef, String> {
    // 字符串字面量："..."
    if expr.starts_with('"') {
        if !expr.ends_with('"') || expr.len() < 2 {
            return Err(format!("字符串字面量未闭合: {expr}"));
        }
        return Ok(VarDef::Literal {
            name: name.to_string(),
            value: expr[1..expr.len() - 1].to_string(),
        });
    }
    // 函数调用：name(a, b, c)
    if let Some(open) = expr.find('(') {
        if !expr.ends_with(')') {
            return Err(format!("函数调用缺右括号: {expr}"));
        }
        let fname = expr[..open].trim();
        let args_raw = expr[open + 1..expr.len() - 1].trim();
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
            ("cur_phase_bucket", 2..=3) => Ok(VarDef::PhaseBucket {
                name: name.to_string(),
                period_s: num(args[0])?,
                bucket_s: num(args[1])?,
                offset_slots: 0,
                prefix: prefix_arg(args.get(2).copied())?,
            }),
            ("next_phase_bucket", 2..=3) => Ok(VarDef::PhaseBucket {
                name: name.to_string(),
                period_s: num(args[0])?,
                bucket_s: num(args[1])?,
                offset_slots: 1,
                prefix: prefix_arg(args.get(2).copied())?,
            }),
            ("cur_phase_bucket" | "next_phase_bucket", n) => Err(format!(
                "{fname}() 需 2~3 参数 (period_s, bucket_s[, prefix])，实际 {n}"
            )),
            _ => Err(format!("未知 VEL 函数: {fname}")),
        }
    } else {
        Err(format!(
            "不支持的表达式: {expr}（支持 字符串字面量 / VEL 内建函数）"
        ))
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

/// 把 `$name` 占位符替换为对应值（文本替换；值不应含 `$`）。
pub fn resolve_vars(sql: &str, vars: &[(String, String)]) -> String {
    let mut out = sql.to_string();
    for (name, value) in vars {
        out = out.replace(&format!("${name}"), value);
    }
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
$cur  = cur_phase_bucket(240, 15)
$next = next_phase_bucket(240, 15)
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
        let kv = eval("$b = cur_phase_bucket(240, 15, \"slot\")", ns(120)).unwrap();
        assert_eq!(kv, vec![("b".to_string(), "slot8".to_string())]);
        // 未知函数 / 缺 = / 空代码 / 纯注释
        assert!(eval("$x = foo(1)", ns(0)).is_err(), "未知函数应报错");
        assert!(eval("no_assign", ns(0)).is_err(), "缺 = 应报错");
        assert!(eval("", ns(0)).unwrap().is_empty());
        assert!(eval("# 纯注释", ns(0)).unwrap().is_empty());
        // 非法相位参数 → 求值报错
        assert!(eval("$x = cur_phase_bucket(15, 240)", ns(0)).is_err());
    }

    #[test]
    fn render_substitutes_and_empty_passes_through() {
        let code = "$cur = cur_phase_bucket(240, 15)\n$max_age = \"2 hours\"";
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
}
