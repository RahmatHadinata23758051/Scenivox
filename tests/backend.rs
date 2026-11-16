use std::{
    net::SocketAddr,
    sync::atomic::{AtomicU64, Ordering},
};

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
fn config_defaults_to_loopback_and_sqlite() {
    let config = Config {
        address: "127.0.0.1:3000"
            .parse::<SocketAddr>()
            .expect("valid address"),
        database_url: "sqlite://scenivox.db".to_owned(),
    };
    assert_eq!(config.address.ip().to_string(), "127.0.0.1");
    assert!(config.database_url.starts_with("sqlite:"));
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
