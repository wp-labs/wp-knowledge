use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use postgres::{Client, NoTls};
use testcontainers::runners::SyncRunner;
use testcontainers_modules::postgres::Postgres;
use wp_knowledge::facade as kdb;
use wp_model_core::model::DataField;

fn temp_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".tmp")
        .join(format!(
            "postgres-testcontainers-{nanos}-{}",
            std::process::id()
        ));
    fs::create_dir_all(&dir).expect("create temporary test directory");
    dir
}

fn connect_with_retry(url: &str) -> Client {
    let mut last_err = None;
    for _ in 0..30 {
        match Client::connect(url, NoTls) {
            Ok(client) => return client,
            Err(err) => {
                last_err = Some(err);
                thread::sleep(Duration::from_secs(1));
            }
        }
    }
    panic!(
        "connect postgres container failed: {}",
        last_err.expect("postgres connect error")
    );
}

#[test]
#[ignore = "requires docker and may need to pull postgres image"]
fn postgres_provider_query_and_pool_via_testcontainers() {
    let container = Postgres::default()
        .start()
        .expect("start postgres testcontainer");
    let port = container
        .get_host_port_ipv4(5432)
        .expect("resolve postgres mapped port");
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let mut admin = connect_with_retry(&url);
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
        .expect("seed postgres test data");

    let tmp = temp_test_dir();
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
    .expect("write knowdb postgres config");

    let authority_uri = format!(
        "file:{}?mode=rwc&uri=true",
        tmp.join("unused.sqlite").display()
    );
    kdb::init_thread_cloned_from_knowdb(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).as_path(),
        &conf_path,
        &authority_uri,
        &orion_variate::EnvDict::default(),
    )
    .expect("init postgres provider from knowdb");

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
}
