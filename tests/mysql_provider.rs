use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use mysql::prelude::Queryable;
use mysql::{Opts, Pool};
use wp_knowledge::facade as kdb;
use wp_model_core::model::DataField;

fn mysql_url() -> String {
    std::env::var("WP_KDB_TEST_MYSQL_URL")
        .expect("WP_KDB_TEST_MYSQL_URL must be set to run mysql_provider")
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn ensure_mysql_provider_initialized() -> String {
    static INIT: OnceLock<String> = OnceLock::new();
    INIT.get_or_init(|| {
        let url = mysql_url();
        let opts = Opts::from_url(&url).expect("parse WP_KDB_TEST_MYSQL_URL failed");
        let pool = Pool::new(opts).expect("connect to WP_KDB_TEST_MYSQL_URL failed");
        let mut admin = pool.get_conn().expect("open mysql admin connection failed");
        admin
            .query_drop(
                r#"
CREATE TABLE IF NOT EXISTS wp_knowledge_mysql_lookup (
    id BIGINT PRIMARY KEY,
    name TEXT NOT NULL,
    pinying TEXT NOT NULL
)
"#,
            )
            .expect("create mysql_provider test table failed");
        admin
            .query_drop("TRUNCATE TABLE wp_knowledge_mysql_lookup")
            .expect("truncate mysql_provider test table failed");
        admin
            .query_drop(
                r#"
INSERT INTO wp_knowledge_mysql_lookup (id, name, pinying)
VALUES
    (1, '令狐冲', 'linghuchong'),
    (2, '小龙女', 'xiaolongnu')
"#,
            )
            .expect("seed mysql_provider test data failed");

        let root = workspace_root();
        let tmp = root.join(".tmp");
        fs::create_dir_all(&tmp).expect("create .tmp for mysql_provider failed");
        let conf_path = tmp.join("mysql-knowdb.toml");
        fs::write(
            &conf_path,
            format!(
                r#"
version = 2

[provider]
kind = "mysql"
connection_uri = "{url}"
pool_size = 8
"#
            ),
        )
        .expect("write mysql_provider knowdb config failed");

        let authority_uri = format!(
            "file:{}?mode=rwc&uri=true",
            tmp.join("unused.sqlite").display()
        );
        kdb::init_thread_cloned_from_knowdb(
            &root,
            &conf_path,
            &authority_uri,
            &orion_variate::EnvDict::default(),
        )
        .expect("init mysql provider from knowdb failed");

        url
    })
    .clone()
}

#[test]
#[ignore = "requires WP_KDB_TEST_MYSQL_URL and a reachable MySQL instance"]
fn mysql_provider_query_and_pool() {
    let _url = ensure_mysql_provider_initialized();

    let params = [DataField::from_chars(
        ":name".to_string(),
        "令狐冲".to_string(),
    )];
    let row = kdb::query_fields(
        "SELECT pinying FROM wp_knowledge_mysql_lookup WHERE name=:name",
        &params,
    )
    .expect("mysql named query");
    assert_eq!(row.len(), 1);
    assert_eq!(row[0].get_name(), "pinying");
    assert_eq!(row[0].to_string(), "chars(linghuchong)");

    let started = Instant::now();
    thread::scope(|scope| {
        for _ in 0..6 {
            scope.spawn(|| {
                let params = [DataField::from_chars(
                    ":name".to_string(),
                    "令狐冲".to_string(),
                )];
                let row = kdb::query_fields(
                    r#"
SELECT pinying
FROM wp_knowledge_mysql_lookup
CROSS JOIN (SELECT SLEEP(0.2)) AS wait_for_pool
WHERE name=:name
"#,
                    &params,
                )
                .expect("concurrent mysql query");
                assert_eq!(row[0].to_string(), "chars(linghuchong)");
            });
        }
    });

    assert!(
        started.elapsed() < Duration::from_millis(900),
        "mysql pooled queries look serialized: {:?}",
        started.elapsed()
    );
}
