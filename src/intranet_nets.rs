//! 内网网段知识管理
//!
//! 内网网段（哪些 IP 段属于内网）属于知识信息，由本模块统一管理。
//! 配置直接放在 `knowdb.toml` 的 `[intranet_nets]` 节（与知识库其他配置同源），
//! 由 knowdb 加载流程解析后通过 [`set_intranet_nets_conf`] 注入；
//! 底层以内存 `ipnet` 集合实现（量小、高频判断），规则引擎（wp-oml）通过
//! [`is_intranet`] 消费。
//!
//! **时序约定**：`INTRANET_NETS_SET` 在首次调用 [`is_intranet`] 时按已注入的配置
//! 初始化。因此 knowdb 加载（注入配置）必须先于任何内网判断调用——引擎流程已保证
//! 这一点（knowdb 在规则执行前加载）；若工具在 knowdb 加载前调用 [`is_intranet`]，
//! 将得到内置默认网段（无外部配置）。

use once_cell::sync::Lazy;
use serde::Deserialize;
use std::net::IpAddr;
use std::path::Path;
use std::str::FromStr;
use std::sync::RwLock;

use ipnet::{IpNet, Ipv4Net, Ipv6Net};

/// 配置合并模式
#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum IntranetMergeMode {
    /// 添加模式：外部配置网段添加到内置默认网段（默认）
    #[default]
    Add,
    /// 替换模式：外部配置网段完全替换内置默认网段
    Replace,
}

/// 内网网段配置（`knowdb.toml` 的 `[intranet_nets]` 节）
///
/// 版本继承 `knowdb.toml` 文件级 `version = 2`，本节不再单独声明。
#[derive(Debug, Deserialize, Clone)]
pub struct IntranetNetsConf {
    /// 外部配置是否生效（默认 true，即提供配置即生效；设为 false 忽略外部网段）
    #[serde(default = "default_intranet_nets_enabled", alias = "enable")]
    pub enabled: bool,
    #[serde(default)]
    pub mode: IntranetMergeMode,
    #[serde(default)]
    pub nets: Vec<String>,
}

fn default_intranet_nets_enabled() -> bool {
    true
}

/// 外部配置注入（由 knowdb.toml 加载流程调用）
static INTRANET_NETS_CONF: RwLock<Option<IntranetNetsConf>> = RwLock::new(None);

/// 设置内网网段配置（knowdb 加载流程解析 `[intranet_nets]` 节后调用）
pub fn set_intranet_nets_conf(conf: Option<IntranetNetsConf>) {
    if let Ok(mut guard) = INTRANET_NETS_CONF.write() {
        *guard = conf;
    }
}

/// 内网 Nets 默认网段全集（RFC1918 + IPv4/IPv6 loopback + IPv6 ULA）
///
/// 注意：CGNAT（100.64.0.0/10）、link-local（169.254.0.0/16、fe80::/10）等
/// 既非内网也非互联网的特殊地址不包含在默认集合内，由外部配置按需扩展。
const DEFAULT_INTRANET_NETS: &[&str] = &[
    "10.0.0.0/8",     // RFC1918
    "172.16.0.0/12",  // RFC1918
    "192.168.0.0/16", // RFC1918
    "127.0.0.0/8",    // IPv4 loopback
    "::1/128",        // IPv6 loopback
    "fc00::/7",       // IPv6 ULA
];

/// 内网 Nets 运行时集合（内存判断，非表机制）
#[derive(Debug)]
pub struct IntranetNetsSet {
    /// IPv4 网段桶
    v4_nets: Vec<Ipv4Net>,
    /// IPv6 网段桶
    v6_nets: Vec<Ipv6Net>,
}

impl IntranetNetsSet {
    /// 创建内置默认网段集合（RFC1918 + IPv4/IPv6 loopback + IPv6 ULA）
    pub fn builtin() -> Self {
        let (v4_nets, v6_nets) = split_nets(
            DEFAULT_INTRANET_NETS
                .iter()
                .filter_map(|s| IpNet::from_str(s).ok()),
        );
        Self { v4_nets, v6_nets }
    }

    /// 合并外部配置（非法网段静默忽略）
    pub fn merge(&mut self, conf: IntranetNetsConf) {
        let (v4, v6) = split_nets(
            conf.nets
                .iter()
                .filter_map(|c| IpNet::from_str(c.trim()).ok()),
        );
        match conf.mode {
            IntranetMergeMode::Add => {
                self.v4_nets.extend(v4);
                self.v6_nets.extend(v6);
            }
            IntranetMergeMode::Replace => {
                self.v4_nets = v4;
                self.v6_nets = v6;
            }
        }
    }

    /// 判断 IP 是否命中内网网段（按地址族分桶，仅扫描对应家族，避免全量扫描）
    pub fn contains(&self, ip: &IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => self.v4_nets.iter().any(|n| n.contains(v4)),
            IpAddr::V6(v6) => self.v6_nets.iter().any(|n| n.contains(v6)),
        }
    }

    /// 当前网段数量（供 init 诊断输出）
    pub fn net_count(&self) -> usize {
        self.v4_nets.len() + self.v6_nets.len()
    }
}

/// 按地址族将网段分桶（IPv4 / IPv6）
fn split_nets<I>(nets: I) -> (Vec<Ipv4Net>, Vec<Ipv6Net>)
where
    I: IntoIterator<Item = IpNet>,
{
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    for net in nets {
        match net {
            IpNet::V4(x) => v4.push(x),
            IpNet::V6(x) => v6.push(x),
        }
    }
    (v4, v6)
}

/// 全局内网 Nets 集合（Lazy 延迟加载：内置默认 + knowdb 注入的外部配置合并）
pub static INTRANET_NETS_SET: Lazy<IntranetNetsSet> = Lazy::new(|| {
    let mut set = IntranetNetsSet::builtin();

    if let Ok(guard) = INTRANET_NETS_CONF.read()
        && let Some(conf) = guard.as_ref()
        && conf.enabled
    {
        set.merge(conf.clone());
    }

    set
});

/// 判断 IP 是否为内网地址（命中内网网段集合）
///
/// IPv4-mapped IPv6（`::ffff:a.b.c.d`）按 IPv4 判定，避免映射地址被误判为外网。
pub fn is_intranet(ip: &IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(*ip),
        _ => *ip,
    };
    INTRANET_NETS_SET.contains(&ip)
}

/// 启动预加载（触发 INTRANET_NETS_SET 初始化），返回诊断消息
///
/// 供引擎启动诊断/运维调用；正常规则执行无需显式调用（Lazy 在首次判断时自然初始化）。
pub fn init_intranet_nets() -> Result<String, String> {
    let set = &*INTRANET_NETS_SET;
    let external = INTRANET_NETS_CONF
        .read()
        .map(|g| g.is_some())
        .unwrap_or(false);

    if external {
        Ok(format!(
            "内网 Nets 已加载 | 外部配置生效 | 网段数: {}",
            set.net_count()
        ))
    } else {
        Ok(format!(
            "内网 Nets 已加载 | 使用内置网段 | 网段数: {}",
            set.net_count()
        ))
    }
}

/// 从 `knowdb.toml` 检查 `[intranet_nets]` 节（供 wproj check）
///
/// 直接从文件解析，不依赖引擎加载；返回 `Ok(None)` 表示未配置（使用内置网段），
/// `Ok(Some(msg))` 表示外部配置生效。
pub fn check_intranet_nets_config(config_path: &Path) -> Result<Option<String>, String> {
    let content =
        std::fs::read_to_string(config_path).map_err(|e| format!("读取 knowdb 配置失败: {}", e))?;
    let value: toml::Value =
        toml::from_str(&content).map_err(|e| format!("解析 knowdb 配置失败: {}", e))?;
    let Some(section) = value.get("intranet_nets") else {
        return Ok(None);
    };
    let conf: IntranetNetsConf = section
        .clone()
        .try_into()
        .map_err(|e| format!("解析 [intranet_nets] 节失败: {}", e))?;
    if !conf.enabled {
        return Ok(None);
    }
    let mode = match conf.mode {
        IntranetMergeMode::Add => "add",
        IntranetMergeMode::Replace => "replace",
    };
    Ok(Some(format!(
        "内网网段配置生效 | {} 个网段 | mode={}",
        conf.nets.len(),
        mode
    )))
}

/// 生成内网网段配置的 `[intranet_nets]` 节（供项目初始化拼入 `knowdb.toml`）
pub fn generate_default_intranet_nets_config() -> String {
    r#"
[intranet_nets]
# 内网网段知识配置：扩展或替换系统内置网段（RFC1918 + IPv4/IPv6 loopback + IPv6 ULA）
#   enabled：外部配置开关（默认 true）
#   mode："add" 添加到内置网段（推荐）/ "replace" 完全替换
#   nets：内网网段列表（CIDR 写法，支持 IPv4 / IPv6）
#         示例：nets = ["172.32.0.0/16"]（企业专有/资产网段）
enabled = true
mode = "add"
nets = []
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn v4(octets: [u8; 4]) -> IpAddr {
        IpAddr::V4(Ipv4Addr::from(octets))
    }

    fn v6(segments: [u16; 8]) -> IpAddr {
        IpAddr::V6(Ipv6Addr::from(segments))
    }

    #[test]
    fn builtin_set_contains_private_ipv4() {
        let set = IntranetNetsSet::builtin();
        assert!(set.contains(&v4([10, 0, 0, 1])));
        assert!(set.contains(&v4([172, 16, 0, 1])));
        assert!(set.contains(&v4([192, 168, 1, 1])));
        assert!(set.contains(&v4([127, 0, 0, 1])));
    }

    #[test]
    fn builtin_set_excludes_public_and_special_ipv4() {
        let set = IntranetNetsSet::builtin();
        assert!(!set.contains(&v4([8, 8, 8, 8])));
        assert!(!set.contains(&v4([172, 32, 0, 1]))); // 172.32 不在 RFC1918 12 位内
        assert!(!set.contains(&v4([11, 0, 0, 1])));
        // CGNAT / link-local 属特殊地址，默认不判为内网
        assert!(!set.contains(&v4([100, 64, 1, 1])));
        assert!(!set.contains(&v4([169, 254, 1, 1])));
    }

    #[test]
    fn builtin_set_contains_private_ipv6() {
        let set = IntranetNetsSet::builtin();
        // fc00::/7 ULA
        assert!(set.contains(&v6([0xfc00, 0, 0, 0, 0, 0, 0, 1])));
        assert!(set.contains(&v6([0xfd00, 0, 0, 0, 0, 0, 0, 1])));
        // ::1 loopback
        assert!(set.contains(&v6([0, 0, 0, 0, 0, 0, 0, 1])));
    }

    #[test]
    fn builtin_set_excludes_public_and_special_ipv6() {
        let set = IntranetNetsSet::builtin();
        assert!(!set.contains(&v6([0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888])));
        assert!(!set.contains(&v6([0x2001, 0xdb8, 0, 0, 0, 0, 0, 1])));
        // fe80::/10 link-local 默认不判为内网
        assert!(!set.contains(&v6([0xfe80, 0, 0, 0, 0, 0, 0, 1])));
    }

    #[test]
    fn merge_add_appends_custom_nets() {
        let mut set = IntranetNetsSet::builtin();
        set.merge(IntranetNetsConf {
            enabled: true,
            mode: IntranetMergeMode::Add,
            nets: vec!["172.32.0.0/16".to_string(), "192.0.2.0/24".to_string()],
        });
        assert!(set.contains(&v4([172, 32, 0, 1])));
        assert!(set.contains(&v4([192, 0, 2, 1])));
        // 默认网段仍保留
        assert!(set.contains(&v4([10, 0, 0, 1])));
    }

    #[test]
    fn merge_replace_overrides_builtin() {
        let mut set = IntranetNetsSet::builtin();
        set.merge(IntranetNetsConf {
            enabled: true,
            mode: IntranetMergeMode::Replace,
            nets: vec!["172.32.0.0/16".to_string()],
        });
        assert!(set.contains(&v4([172, 32, 0, 1])));
        assert!(!set.contains(&v4([10, 0, 0, 1]))); // 默认网段被替换
    }

    #[test]
    fn merge_ignores_invalid_nets() {
        let mut set = IntranetNetsSet::builtin();
        let before = set.net_count();
        set.merge(IntranetNetsConf {
            enabled: true,
            mode: IntranetMergeMode::Add,
            nets: vec!["not-a-cidr".to_string(), "10.0.0.0/999".to_string()],
        });
        assert_eq!(set.net_count(), before, "非法 Nets 应被忽略");
    }

    #[test]
    fn toml_enabled_defaults_to_true() {
        // 未写 enabled 的配置 → 默认 true（提供配置即生效）
        let toml_str = r#"
            nets = ["172.32.0.0/16"]
        "#;
        let conf: IntranetNetsConf = toml::from_str(toml_str).unwrap();
        assert!(conf.enabled);
        assert_eq!(conf.nets, vec!["172.32.0.0/16".to_string()]);
    }

    #[test]
    fn toml_explicit_enabled_false_ignored() {
        let toml_str = r#"
            enabled = false
            nets = ["172.32.0.0/16"]
        "#;
        let conf: IntranetNetsConf = toml::from_str(toml_str).unwrap();
        assert!(!conf.enabled);
    }

    #[test]
    fn is_intranet_ipv4_mapped_ipv6() {
        // ::ffff:192.168.0.1 → 按 IPv4 判定为内网
        let mapped: IpAddr = "::ffff:192.168.0.1".parse().unwrap();
        assert!(is_intranet(&mapped));
        let mapped_public: IpAddr = "::ffff:8.8.8.8".parse().unwrap();
        assert!(!is_intranet(&mapped_public));
    }

    #[test]
    fn generate_default_config_contains_nets_field() {
        let cfg = generate_default_intranet_nets_config();
        assert!(cfg.contains("[intranet_nets]"));
        assert!(cfg.contains("nets ="));
        assert!(cfg.contains("172.32.0.0/16"));
    }

    #[test]
    fn check_intranet_nets_config_reads_section() {
        let path = std::env::temp_dir().join(format!(
            "wpk_intranet_check_section_{}.toml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            "version = 2\n[intranet_nets]\nenabled = true\nmode = \"add\"\nnets = [\"172.32.0.0/16\"]\n",
        )
        .unwrap();
        let msg = check_intranet_nets_config(&path)
            .unwrap()
            .expect("should report config effective");
        assert!(msg.contains("1 个网段"), "msg: {}", msg);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn check_intranet_nets_config_missing_section() {
        let path = std::env::temp_dir().join(format!(
            "wpk_intranet_check_missing_{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, "version = 2\n").unwrap();
        let r = check_intranet_nets_config(&path).unwrap();
        assert!(r.is_none());
        std::fs::remove_file(&path).ok();
    }
}
