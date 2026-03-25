use std::path::PathBuf;

use once_cell::sync::OnceCell;
use orion_variate::EnvDict;
use wp_knowledge::facade as kdb;
use wp_model_core::model::DataField;

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points to crates/wp-knowledge；这里直接返回该路径
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn ensure_packaged_knowdb_initialized() -> PathBuf {
    static INIT: OnceCell<()> = OnceCell::new();
    let root = workspace_root();
    let root_clone = root.clone();
    INIT.get_or_init(|| {
        let conf_path = root_clone.join("knowdb/knowdb.toml");
        let authority_file = root_clone.join(".run/authority_test.sqlite");
        if let Some(parent) = authority_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::remove_file(&authority_file);
        let authority_uri = format!("file:{}?mode=rwc&uri=true", authority_file.display());
        kdb::init_thread_cloned_from_knowdb(
            &root_clone,
            &conf_path,
            &authority_uri,
            &EnvDict::default(),
        )
        .expect("init knowdb v2");
    });
    root
}

#[test]
fn load_knowdb_v2_and_query() {
    let _root = ensure_packaged_knowdb_initialized();

    // 1) 命名参数查询 example
    let params = [DataField::from_chars(":name".to_string(), "令狐冲".to_string())];
    let rows =
        kdb::query_fields("SELECT pinying FROM example WHERE name=:name", &params).expect("query example");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get_name(), "pinying");
    assert_eq!(rows[0].to_string(), "chars(linghuchong)");

    let compat_params = [(":name", &"令狐冲" as &dyn rusqlite::ToSql)];
    let compat_rows = kdb::query_named("SELECT pinying FROM example WHERE name=:name", &compat_params)
        .expect("query example via compatibility wrapper");
    assert_eq!(compat_rows.len(), 1);
    assert_eq!(compat_rows[0].to_string(), "chars(linghuchong)");

    // 2) 读取词表 address（query_cipher）
    let vals = kdb::query_cipher("address").expect("cipher address");
    assert!(vals.iter().any(|v| v == "address_0"));

    // 3) 白名单拦截未知表
    let err = kdb::query_cipher("not_exist").expect_err("deny unknown table");
    let msg = format!("{}", err);
    assert!(msg.contains("not allowed"));
}

#[test]
fn query_zone_table_segments() {
    let _root = ensure_packaged_knowdb_initialized();

    let query_ip = "10.0.74.45";
    let ip_param = [DataField::from_chars(":ip".to_string(), query_ip.to_string())];
    let rows = kdb::query_fields(
        "SELECT zone FROM zone WHERE ip4_between(:ip, start_ip_int, end_ip_int)=1 LIMIT 1",
        &ip_param,
    )
    .expect("zone lookup (ip4_between)");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].to_string(), "chars(work_zone)");

    let rows_range = kdb::query_fields(
        "SELECT zone FROM zone WHERE start_ip_int < ip4_int(:ip) AND end_ip_int >= ip4_int(:ip) LIMIT 1",
        &ip_param,
    )
    .expect("zone lookup (range compare)");
    assert_eq!(rows_range.len(), 1);
    assert_eq!(rows_range[0].to_string(), "chars(work_zone)");

    let count = kdb::query_row("SELECT COUNT(*) AS total FROM zone").expect("row count");
    assert_eq!(count.len(), 1);
    assert_eq!(count[0].to_string(), "digit(4)");
}
