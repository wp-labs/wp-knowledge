use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use postgres::{Client, NoTls};
use wp_knowledge::facade as kdb;
use wp_model_core::model::DataField;

fn postgres_url() -> String {
    std::env::var("WP_KDB_TEST_POSTGRES_URL")
        .expect("WP_KDB_TEST_POSTGRES_URL must be set to run postgres_provider")
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn ensure_postgres_provider_initialized() -> String {
    static INIT: OnceLock<String> = OnceLock::new();
    INIT.get_or_init(|| {
        let url = postgres_url();

        let mut admin =
            Client::connect(&url, NoTls).expect("connect to WP_KDB_TEST_POSTGRES_URL failed");
        admin
            .batch_execute(
                r#"
CREATE TABLE IF NOT EXISTS wp_knowledge_pg_lookup (
    id BIGINT PRIMARY KEY,
    name TEXT NOT NULL,
    pinying TEXT NOT NULL
);
TRUNCATE TABLE wp_knowledge_pg_lookup;
INSERT INTO wp_knowledge_pg_lookup (id, name, pinying)
VALUES
    (1, '令狐冲', 'linghuchong'),
    (2, '小龙女', 'xiaolongnu');
"#,
            )
            .expect("seed postgres_provider test data failed");

        let root = workspace_root();
        let tmp = root.join(".tmp");
        fs::create_dir_all(&tmp).expect("create .tmp for postgres_provider failed");
        let conf_path = tmp.join("postgres-knowdb.toml");
        fs::write(
            &conf_path,
            format!(
                r#"
version = 2

[provider]
kind = "postgres"
connection_uri = "{url}"
"#
            ),
        )
        .expect("write postgres_provider knowdb config failed");

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
        .expect("init postgres provider from knowdb failed");

        url
    })
    .clone()
}

#[test]
#[ignore = "requires WP_KDB_TEST_POSTGRES_URL and a reachable PostgreSQL instance"]
fn postgres_provider_query_and_cipher() {
    let _url = ensure_postgres_provider_initialized();

    let params = [DataField::from_chars(
        ":name".to_string(),
        "令狐冲".to_string(),
    )];
    let row = kdb::query_fields(
        "SELECT pinying FROM wp_knowledge_pg_lookup WHERE name=:name",
        &params,
    )
    .expect("postgres named query");
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
WITH wait_for_pool AS (SELECT pg_sleep(0.2))
SELECT pinying
FROM wp_knowledge_pg_lookup
CROSS JOIN wait_for_pool
WHERE name=:name
"#,
                    &params,
                )
                .expect("concurrent postgres query");
                assert_eq!(row[0].to_string(), "chars(linghuchong)");
            });
        }
    });

    assert!(
        started.elapsed() < Duration::from_millis(900),
        "postgres pooled queries look serialized: {:?}",
        started.elapsed()
    );
}
