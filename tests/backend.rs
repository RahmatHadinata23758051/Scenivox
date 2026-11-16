use std::sync::atomic::{AtomicU64, Ordering};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use scenivox::{api, config::Config, db};
use tower::ServiceExt;

static NEXT_DB: AtomicU64 = AtomicU64::new(0);

fn test_database_url() -> String {
    let path = std::env::temp_dir().join(format!(
        "scenivox-test-{}-{}.db",
        std::process::id(),
        NEXT_DB.fetch_add(1, Ordering::Relaxed)
    ));
    format!(
        "sqlite://{}?mode=rwc",
        path.to_string_lossy().replace('\\', "/")
    )
}

#[test]
fn config_parses_defaults_and_rejects_invalid_settings() {
    let config = Config::from_values(None, None).expect("valid defaults");
    assert!(config.address.ip().is_loopback());
    assert!(Config::from_values(Some("not-an-address"), None).is_err());
    assert!(Config::from_values(None, Some("postgres://localhost/db")).is_err());
}

#[tokio::test]
async fn migration_creates_metadata_table_and_health_reports_ok() {
    let pool = db::connect(&test_database_url())
        .await
        .expect("migrations run");
    let table_exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'app_metadata'",
    )
    .fetch_one(&pool)
    .await
    .expect("metadata table query");
    assert_eq!(table_exists, 1);

    let response = api::router(pool.clone())
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("health response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    assert_eq!(bytes, r#"{"status":"ok"}"#);
    pool.close().await;
}

#[tokio::test]
async fn unknown_route_is_not_found() {
    let pool = db::connect(&test_database_url())
        .await
        .expect("migrations run");
    let response = api::router(pool.clone())
        .oneshot(
            Request::builder()
                .uri("/missing")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    pool.close().await;
}

#[tokio::test]
async fn data_and_migration_history_survive_reconnect() {
    let url = test_database_url();
    let pool = db::connect(&url).await.expect("initial migration");
    sqlx::query("INSERT INTO app_metadata (key, value) VALUES (?, ?)")
        .bind("persistence-probe")
        .bind("retained after reconnect")
        .execute(&pool)
        .await
        .expect("write metadata");
    pool.close().await;

    let reopened = db::connect(&url)
        .await
        .expect("idempotent migration on reconnect");
    let value: String = sqlx::query_scalar("SELECT value FROM app_metadata WHERE key = ?")
        .bind("persistence-probe")
        .fetch_one(&reopened)
        .await
        .expect("read persisted metadata");
    assert_eq!(value, "retained after reconnect");
    let migrations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations WHERE success = 1")
            .fetch_one(&reopened)
            .await
            .expect("migration history");
    assert_eq!(migrations, 2);
    reopened.close().await;
    std::fs::remove_file(
        url.strip_prefix("sqlite://")
            .unwrap()
            .strip_suffix("?mode=rwc")
            .unwrap(),
    )
    .expect("remove test database");
}
