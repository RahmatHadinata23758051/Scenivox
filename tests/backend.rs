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
    assert_eq!(migrations, 6);
    reopened.close().await;
    std::fs::remove_file(
        url.strip_prefix("sqlite://")
            .unwrap()
            .strip_suffix("?mode=rwc")
            .unwrap(),
    )
    .expect("remove test database");
}

#[tokio::test]
async fn storyboard_generation_route_persists_deterministic_draft() {
    let pool = db::connect(&test_database_url())
        .await
        .expect("migrations run");
    let project = scenivox::store::Store::new(pool.clone())
        .create_project(serde_json::from_value(serde_json::json!({
            "schema_version":"1",
            "title":"Planner route test",
            "idea":"A seed grows",
            "target":{"duration_ms":30000,"aspect_ratio":"9:16","fps":30,"language":"id-ID","style_id":"stick-figure"},
            "model_selection":{}
        })).unwrap())
        .await
        .unwrap();
    let body_value = serde_json::json!({
        "schema_version":"1",
        "idea":"A seed grows",
        "target":project.target,
        "expected_revision_id":null,
        "model_selection":{}
    });
    let body = body_value.to_string();
    let response = api::router(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/projects/{}/storyboard:generate", project.id))
                .header("content-type", "application/json")
                .header("idempotency-key", "00000000-0000-4000-8000-000000000011")
                .body(Body::from(body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert!(response.headers().contains_key("location"));
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let revision: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        revision["content"]["planning_provenance"]["source"],
        "deterministic_mock"
    );
    let replay = api::router(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/projects/{}/storyboard:generate", project.id))
                .header("content-type", "application/json")
                .header("idempotency-key", "00000000-0000-4000-8000-000000000011")
                .body(Body::from(body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::CREATED);
    let replay_bytes = axum::body::to_bytes(replay.into_body(), usize::MAX)
        .await
        .unwrap();
    let replay_revision: serde_json::Value = serde_json::from_slice(&replay_bytes).unwrap();
    assert_eq!(replay_revision, revision);

    let conflict_body = serde_json::to_string(&serde_json::json!({
        "schema_version":"1",
        "idea":"Different idea",
        "target":project.target,
        "expected_revision_id":null,
        "model_selection":{}
    }))
    .unwrap();
    let conflict = api::router(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/projects/{}/storyboard:generate", project.id))
                .header("content-type", "application/json")
                .header("idempotency-key", "00000000-0000-4000-8000-000000000011")
                .body(Body::from(conflict_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let conflict_bytes = axum::body::to_bytes(conflict.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&conflict_bytes).contains("idempotency_conflict"));

    let stale = api::router(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/projects/{}/storyboard:generate", project.id))
                .header("content-type", "application/json")
                .header("idempotency-key", "00000000-0000-4000-8000-000000000012")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    let stale_bytes = axum::body::to_bytes(stale.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&stale_bytes).contains("stale_revision"));
    pool.close().await;
}

#[tokio::test]
async fn revision_review_and_approval_routes_enforce_hashes_and_replay() {
    let pool = db::connect(&test_database_url()).await.unwrap();
    let store = scenivox::store::Store::new(pool.clone());
    let project = store.create_project(serde_json::from_value(serde_json::json!({
        "schema_version":"1","title":"Review route","idea":"Review test",
        "target":{"duration_ms":60000,"aspect_ratio":"9:16","fps":30,"language":"id-ID","style_id":"stick-figure"},"model_selection":{}
    })).unwrap()).await.unwrap();
    let revision = store
        .create_revision(
            &project.id,
            serde_json::from_str(include_str!("fixtures/p1-revision.json")).unwrap(),
        )
        .await
        .unwrap();
    let uri = |action: &str| {
        format!(
            "/v1/projects/{}/revisions/{}/{}",
            project.id, revision.id, action
        )
    };
    let body = serde_json::json!({"schema_version":"1","content_sha256":revision.content_sha256})
        .to_string();
    let request = |action: &str, key: &str, payload: String| {
        Request::builder()
            .method("POST")
            .uri(uri(action))
            .header("content-type", "application/json")
            .header("idempotency-key", key)
            .body(Body::from(payload))
            .unwrap()
    };

    let before_review = api::router(pool.clone())
        .oneshot(request(
            "approve",
            "00000000-0000-4000-8000-000000000081",
            body.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(before_review.status(), StatusCode::CONFLICT);
    let before_review_bytes = axum::body::to_bytes(before_review.into_body(), usize::MAX)
        .await
        .unwrap();
    let before_review_error: serde_json::Value =
        serde_json::from_slice(&before_review_bytes).unwrap();
    assert_eq!(before_review_error["error"]["code"], "invalid_transition");
    let bad_hash =
        serde_json::json!({"schema_version":"1","content_sha256":"0".repeat(64)}).to_string();
    let mismatch = api::router(pool.clone())
        .oneshot(request(
            "review",
            "00000000-0000-4000-8000-000000000082",
            bad_hash,
        ))
        .await
        .unwrap();
    assert_eq!(mismatch.status(), StatusCode::CONFLICT);
    let mismatch_bytes = axum::body::to_bytes(mismatch.into_body(), usize::MAX)
        .await
        .unwrap();
    let mismatch_error: serde_json::Value = serde_json::from_slice(&mismatch_bytes).unwrap();
    assert_eq!(mismatch_error["error"]["code"], "hash_mismatch");
    let review = api::router(pool.clone())
        .oneshot(request(
            "review",
            "00000000-0000-4000-8000-000000000083",
            body.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(review.status(), StatusCode::OK);
    let review_bytes = axum::body::to_bytes(review.into_body(), usize::MAX)
        .await
        .unwrap();
    let review_value: serde_json::Value = serde_json::from_slice(&review_bytes).unwrap();
    assert_eq!(review_value["review_state"], "in_review");
    let review_replay = api::router(pool.clone())
        .oneshot(request(
            "review",
            "00000000-0000-4000-8000-000000000083",
            body.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(review_replay.status(), StatusCode::OK);
    let changed_body =
        serde_json::json!({"schema_version":"1","content_sha256":"0".repeat(64)}).to_string();
    let replay_conflict = api::router(pool.clone())
        .oneshot(request(
            "review",
            "00000000-0000-4000-8000-000000000083",
            changed_body,
        ))
        .await
        .unwrap();
    assert_eq!(replay_conflict.status(), StatusCode::CONFLICT);
    let replay_conflict_bytes = axum::body::to_bytes(replay_conflict.into_body(), usize::MAX)
        .await
        .unwrap();
    let replay_conflict_error: serde_json::Value =
        serde_json::from_slice(&replay_conflict_bytes).unwrap();
    assert_eq!(
        replay_conflict_error["error"]["code"],
        "idempotency_conflict"
    );
    let withdrawal = api::router(pool.clone())
        .oneshot(request(
            "withdraw-review",
            "00000000-0000-4000-8000-000000000088",
            body.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(withdrawal.status(), StatusCode::OK);
    let withdrawal_bytes = axum::body::to_bytes(withdrawal.into_body(), usize::MAX)
        .await
        .unwrap();
    let withdrawal_value: serde_json::Value = serde_json::from_slice(&withdrawal_bytes).unwrap();
    assert_eq!(withdrawal_value["review_state"], "draft");
    let re_review = api::router(pool.clone())
        .oneshot(request(
            "review",
            "00000000-0000-4000-8000-000000000089",
            body.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(re_review.status(), StatusCode::OK);

    let approval = api::router(pool.clone())
        .oneshot(request(
            "approve",
            "00000000-0000-4000-8000-000000000084",
            body.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(approval.status(), StatusCode::CREATED);
    let location = approval
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let approval_bytes = axum::body::to_bytes(approval.into_body(), usize::MAX)
        .await
        .unwrap();
    let approval_value: serde_json::Value = serde_json::from_slice(&approval_bytes).unwrap();
    assert_eq!(approval_value["content_sha256"], revision.content_sha256);
    let duplicate_new_key = api::router(pool.clone())
        .oneshot(request(
            "approve",
            "00000000-0000-4000-8000-000000000085",
            body.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(duplicate_new_key.status(), StatusCode::OK);
    let duplicate_bytes = axum::body::to_bytes(duplicate_new_key.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&duplicate_bytes).unwrap(),
        approval_value
    );
    let replay = api::router(pool.clone())
        .oneshot(request(
            "approve",
            "00000000-0000-4000-8000-000000000084",
            body.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(
        replay.headers().get("location").unwrap().to_str().unwrap(),
        location
    );
    assert_eq!(
        replay.status(),
        StatusCode::CREATED,
        "same-key replay preserves original response status"
    );
    let replay_bytes = axum::body::to_bytes(replay.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&replay_bytes).unwrap(),
        approval_value
    );
    let evidence = api::router(pool.clone())
        .oneshot(
            Request::builder()
                .uri(location)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(evidence.status(), StatusCode::OK);
    let render_request = api::router(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/projects/{}/render-jobs", project.id))
                .header("content-type", "application/json")
                .header("idempotency-key", "00000000-0000-4000-8000-000000000091")
                .body(Body::from(
                    serde_json::json!({
                        "schema_version":"1",
                        "revision_id":revision.id,
                        "content_sha256":revision.content_sha256,
                        "cost_consent_id":null
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(render_request.status(), StatusCode::SERVICE_UNAVAILABLE);
    let render_bytes = axum::body::to_bytes(render_request.into_body(), usize::MAX)
        .await
        .unwrap();
    let render_error: serde_json::Value = serde_json::from_slice(&render_bytes).unwrap();
    assert_eq!(render_error["error"]["code"], "skill_unavailable");
    let evidence_bytes = axum::body::to_bytes(evidence.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&evidence_bytes).unwrap(),
        approval_value
    );
    let withdraw_after_approval = api::router(pool.clone())
        .oneshot(request(
            "withdraw-review",
            "00000000-0000-4000-8000-000000000086",
            body,
        ))
        .await
        .unwrap();
    let child = store.create_revision(&project.id, serde_json::from_value(serde_json::json!({
        "schema_version":"1","expected_revision_id":revision.id.clone(),"content":revision.content.clone()
    })).unwrap()).await.unwrap();
    let stale = api::router(pool.clone())
        .oneshot(request(
            "approve",
            "00000000-0000-4000-8000-000000000090",
            serde_json::json!({"schema_version":"1","content_sha256":revision.content_sha256})
                .to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    let stale_bytes = axum::body::to_bytes(stale.into_body(), usize::MAX)
        .await
        .unwrap();
    let stale_error: serde_json::Value = serde_json::from_slice(&stale_bytes).unwrap();
    assert_eq!(stale_error["error"]["code"], "stale_revision");
    assert_eq!(
        store.get_revision(&project.id, &child.id).await.unwrap(),
        child
    );
    assert_eq!(withdraw_after_approval.status(), StatusCode::CONFLICT);
    let forbidden_bytes = axum::body::to_bytes(withdraw_after_approval.into_body(), usize::MAX)
        .await
        .unwrap();
    let forbidden_error: serde_json::Value = serde_json::from_slice(&forbidden_bytes).unwrap();
    assert_eq!(forbidden_error["error"]["code"], "invalid_transition");

    let other_project = store
        .create_project(serde_json::from_str(include_str!("fixtures/p1-project.json")).unwrap())
        .await
        .unwrap();
    let wrong_project = api::router(pool.clone()).oneshot(Request::builder()
        .method("POST").uri(format!("/v1/projects/{}/revisions/{}/review", other_project.id, revision.id))
        .header("content-type","application/json").header("idempotency-key","00000000-0000-4000-8000-000000000087")
        .body(Body::from(serde_json::json!({"schema_version":"1","content_sha256":revision.content_sha256}).to_string())).unwrap()).await.unwrap();
    assert_eq!(wrong_project.status(), StatusCode::NOT_FOUND);
    let wrong_project_bytes = axum::body::to_bytes(wrong_project.into_body(), usize::MAX)
        .await
        .unwrap();
    let wrong_project_error: serde_json::Value =
        serde_json::from_slice(&wrong_project_bytes).unwrap();
    assert_eq!(wrong_project_error["error"]["code"], "not_found");
    pool.close().await;
}
