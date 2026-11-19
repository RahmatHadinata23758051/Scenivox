use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use scenivox::{
    api, db,
    domain::{
        CreateProject, CreateRenderJob, CreateRevision, JobCancelCommand, RetryRenderJob,
        ReviewCommand,
    },
    store::{Store, StoreError},
};
use sqlx::{Row, SqlitePool};
use tempfile::TempDir;
use tower::ServiceExt;

const PROJECT: &str = include_str!("fixtures/p1-project.json");
const REVISION: &str = include_str!("fixtures/p1-revision.json");

async fn http_enqueue(
    pool: &SqlitePool,
    project_id: &str,
    input: &CreateRenderJob,
) -> axum::response::Response {
    api::router(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/projects/{project_id}/render-jobs"))
                .header("content-type", "application/json")
                .header("idempotency-key", uuid::Uuid::new_v4().to_string())
                .body(Body::from(serde_json::to_vec(input).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn http_error(response: axum::response::Response) -> (StatusCode, String) {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    (status, body["error"]["code"].as_str().unwrap().to_string())
}

#[tokio::test]
async fn http_enqueue_authorization_errors_never_create_jobs() {
    let f = Fixture::new().await;
    let project = f
        .store
        .create_project(serde_json::from_str::<CreateProject>(PROJECT).unwrap())
        .await
        .unwrap();
    let revision = f
        .store
        .create_revision(
            &project.id,
            serde_json::from_str::<CreateRevision>(REVISION).unwrap(),
        )
        .await
        .unwrap();
    let draft =
        http_error(http_enqueue(&f.pool, &project.id, &enqueue(&revision, None)).await).await;
    assert_eq!(draft, (StatusCode::CONFLICT, "approval_required".into()));
    let wrong = f
        .store
        .create_project(serde_json::from_str::<CreateProject>(PROJECT).unwrap())
        .await
        .unwrap();
    let wrong_project =
        http_error(http_enqueue(&f.pool, &wrong.id, &enqueue(&revision, None)).await).await;
    assert_eq!(
        wrong_project,
        (StatusCode::CONFLICT, "stale_revision".into())
    );
    let command = ReviewCommand {
        schema_version: "1".into(),
        content_sha256: revision.content_sha256.clone(),
    };
    f.store
        .review_revision(&project.id, &revision.id, None, command.clone())
        .await
        .unwrap();
    f.store
        .approve_revision(&project.id, &revision.id, None, command)
        .await
        .unwrap();
    let mismatch = CreateRenderJob {
        content_sha256: "0".repeat(64),
        ..enqueue(&revision, None)
    };
    assert_eq!(
        http_error(http_enqueue(&f.pool, &project.id, &mismatch).await).await,
        (StatusCode::CONFLICT, "hash_mismatch".into())
    );
    assert_eq!(
        http_error(
            http_enqueue(
                &f.pool,
                &project.id,
                &enqueue(&revision, Some("unverified"))
            )
            .await
        )
        .await,
        (StatusCode::CONFLICT, "remote_cost_not_approved".into())
    );
    assert_eq!(
        http_error(http_enqueue(&f.pool, &project.id, &enqueue(&revision, None)).await).await,
        (StatusCode::SERVICE_UNAVAILABLE, "skill_unavailable".into())
    );
    let asset_command: CreateRevision = serde_json::from_str(REVISION).unwrap();
    let mut asset_content = asset_command.content;
    asset_content.asset_hashes.push("d".repeat(64));
    let (asset_project, asset_revision, _) = approved(&f, Some(asset_content)).await;
    assert_eq!(
        http_error(http_enqueue(&f.pool, &asset_project.id, &enqueue(&asset_revision, None)).await)
            .await,
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_asset_reference".into()
        )
    );
    let mut next: CreateRevision = serde_json::from_str(REVISION).unwrap();
    next.expected_revision_id = Some(revision.id.clone());
    f.store.create_revision(&project.id, next).await.unwrap();
    assert_eq!(
        http_error(http_enqueue(&f.pool, &project.id, &enqueue(&revision, None)).await).await,
        (StatusCode::CONFLICT, "stale_revision".into())
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM render_jobs")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

struct Fixture {
    _dir: TempDir,
    url: String,
    pool: SqlitePool,
    store: Store,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path()
                .join("render.db")
                .to_string_lossy()
                .replace('\\', "/")
        );
        let pool = db::connect(&url).await.unwrap();
        let store = Store::new(pool.clone());
        Self {
            _dir: dir,
            url,
            pool,
            store,
        }
    }
    fn reopen_url(&self) -> String {
        self.url.replace("mode=rwc", "mode=rw")
    }
}

async fn approved(
    f: &Fixture,
    mut content: Option<scenivox::domain::RevisionInput>,
) -> (
    scenivox::domain::Project,
    scenivox::domain::Revision,
    String,
) {
    let project = f
        .store
        .create_project(serde_json::from_str::<CreateProject>(PROJECT).unwrap())
        .await
        .unwrap();
    let mut command: CreateRevision = serde_json::from_str(REVISION).unwrap();
    if let Some(value) = content.take() {
        command.content = value;
    }
    let revision = f.store.create_revision(&project.id, command).await.unwrap();
    let review = ReviewCommand {
        schema_version: "1".into(),
        content_sha256: revision.content_sha256.clone(),
    };
    f.store
        .review_revision(&project.id, &revision.id, None, review.clone())
        .await
        .unwrap();
    let (approval, _, _) = f
        .store
        .approve_revision(&project.id, &revision.id, None, review)
        .await
        .unwrap();
    (project, revision, approval.id)
}

fn enqueue(revision: &scenivox::domain::Revision, consent: Option<&str>) -> CreateRenderJob {
    CreateRenderJob {
        schema_version: "1".into(),
        revision_id: revision.id.clone(),
        content_sha256: revision.content_sha256.clone(),
        cost_consent_id: consent.map(str::to_owned),
    }
}

async fn seed_job(
    f: &Fixture,
    project: &scenivox::domain::Project,
    revision: &scenivox::domain::Revision,
    approval_id: &str,
    status: &str,
) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let snapshot: String =
        sqlx::query_scalar("SELECT content_json FROM revisions WHERE project_id=? AND id=?")
            .bind(&project.id)
            .bind(&revision.id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    let error = if status == "failed" || status == "interrupted" {
        Some("fixture_error")
    } else {
        None
    };
    let artifacts = if status == "succeeded" {
        "[\"fixture-only-artifact-id\"]"
    } else {
        "[]"
    };
    sqlx::query("INSERT INTO render_jobs(id,project_id,revision_id,approval_id,content_sha256,snapshot_json,status,created_at,error_code,artifact_ids_json,request_sha256) VALUES(?,?,?,?,?,?,?,strftime('%Y-%m-%dT%H:%M:%SZ','now'),?,?,?)")
        .bind(&id).bind(&project.id).bind(&revision.id).bind(approval_id).bind(&revision.content_sha256).bind(snapshot).bind(status).bind(error).bind(artifacts).bind("a".repeat(64)).execute(&f.pool).await.unwrap();
    id
}

#[tokio::test]
async fn store_enqueue_fails_closed_for_unapproved_stale_wrong_project_hash_assets_skill_and_cost()
{
    let f = Fixture::new().await;
    let project = f
        .store
        .create_project(serde_json::from_str::<CreateProject>(PROJECT).unwrap())
        .await
        .unwrap();
    let revision = f
        .store
        .create_revision(
            &project.id,
            serde_json::from_str::<CreateRevision>(REVISION).unwrap(),
        )
        .await
        .unwrap();
    let draft = f
        .store
        .enqueue_render_job(&project.id, None, enqueue(&revision, None))
        .await
        .unwrap_err();
    assert!(matches!(
        draft,
        StoreError::Conflict {
            code: "approval_required"
        }
    ));
    let wrong_project = f
        .store
        .create_project(serde_json::from_str::<CreateProject>(PROJECT).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        f.store
            .enqueue_render_job(&wrong_project.id, None, enqueue(&revision, None))
            .await
            .unwrap_err(),
        StoreError::StaleRevision | StoreError::NotFound
    ));
    let review = ReviewCommand {
        schema_version: "1".into(),
        content_sha256: revision.content_sha256.clone(),
    };
    f.store
        .review_revision(&project.id, &revision.id, None, review.clone())
        .await
        .unwrap();
    f.store
        .approve_revision(&project.id, &revision.id, None, review)
        .await
        .unwrap();
    let bad_hash = CreateRenderJob {
        content_sha256: "0".repeat(64),
        ..enqueue(&revision, None)
    };
    assert!(matches!(
        f.store
            .enqueue_render_job(&project.id, None, bad_hash)
            .await
            .unwrap_err(),
        StoreError::Conflict {
            code: "hash_mismatch"
        }
    ));
    let consent = f
        .store
        .enqueue_render_job(
            &project.id,
            None,
            enqueue(&revision, Some("opaque-unverified-consent")),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        consent,
        StoreError::Conflict {
            code: "remote_cost_not_approved"
        }
    ));
    let skill = f
        .store
        .enqueue_render_job(&project.id, None, enqueue(&revision, None))
        .await
        .unwrap_err();
    assert!(matches!(
        skill,
        StoreError::Conflict {
            code: "skill_unavailable"
        }
    ));

    let asset_command: CreateRevision = serde_json::from_str(REVISION).unwrap();
    let mut asset_content = asset_command.content;
    asset_content.asset_hashes.push("b".repeat(64));
    let (asset_project, asset_revision, _) = approved(&f, Some(asset_content)).await;
    assert!(matches!(
        f.store
            .enqueue_render_job(&asset_project.id, None, enqueue(&asset_revision, None))
            .await
            .unwrap_err(),
        StoreError::Validation {
            code: "invalid_asset_reference",
            ..
        }
    ));

    let sfx_command: CreateRevision = serde_json::from_str(REVISION).unwrap();
    let mut sfx_content = sfx_command.content;
    sfx_content.storyboard.scenes[0]
        .sfx_cues
        .push(scenivox::domain::SfxCue {
            asset_id: uuid::Uuid::new_v4().to_string(),
            at_ms: 0,
        });
    let sfx_project = f
        .store
        .create_project(serde_json::from_str::<CreateProject>(PROJECT).unwrap())
        .await
        .unwrap();
    let mut sfx_revision_command: CreateRevision = serde_json::from_str(REVISION).unwrap();
    sfx_revision_command.content = sfx_content;
    assert!(matches!(
        f.store
            .create_revision(&sfx_project.id, sfx_revision_command)
            .await
            .unwrap_err(),
        StoreError::Validation {
            code: "invalid_asset_reference",
            ..
        }
    ));
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM render_jobs")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "rejected requests must not create work");
}

#[tokio::test]
async fn stale_approved_revision_and_retry_without_prerequisites_are_rejected() {
    let f = Fixture::new().await;
    let (project, revision, approval) = approved(&f, None).await;
    let old_job = seed_job(&f, &project, &revision, &approval, "cancelled").await;
    let retry = f
        .store
        .retry_render_job(
            &old_job,
            RetryRenderJob {
                schema_version: "1".into(),
                cost_consent_id: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(
        retry,
        StoreError::Conflict {
            code: "skill_unavailable"
        }
    ));
    let mut next: CreateRevision = serde_json::from_str(REVISION).unwrap();
    next.expected_revision_id = Some(revision.id.clone());
    let _new_revision = f.store.create_revision(&project.id, next).await.unwrap();
    assert!(matches!(
        f.store
            .enqueue_render_job(&project.id, None, enqueue(&revision, None))
            .await
            .unwrap_err(),
        StoreError::StaleRevision
    ));
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM render_jobs")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "only the explicitly seeded test job exists");
}

#[tokio::test]
async fn job_constraints_cancellation_reopen_and_startup_recovery_are_idempotent() {
    let f = Fixture::new().await;
    let (project, revision, approval) = approved(&f, None).await;
    let queued_cancel = seed_job(&f, &project, &revision, &approval, "queued").await;
    let queued_survives = seed_job(&f, &project, &revision, &approval, "queued").await;
    let active: Vec<String> = ["preparing", "voicing", "animating", "mixing", "validating"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut ids = Vec::new();
    for status in &active {
        ids.push((
            status.clone(),
            seed_job(&f, &project, &revision, &approval, status).await,
        ));
    }
    for status in ["failed", "interrupted", "cancelled", "succeeded"] {
        ids.push((
            status.to_string(),
            seed_job(&f, &project, &revision, &approval, status).await,
        ));
    }

    let bad_snapshot =
        sqlx::query("SELECT content_json FROM revisions WHERE project_id=? AND id=?")
            .bind(&project.id)
            .bind(&revision.id)
            .fetch_one(&f.pool)
            .await
            .unwrap()
            .get::<String, _>(0)
            + " ";
    let rejected = sqlx::query("INSERT INTO render_jobs(id,project_id,revision_id,approval_id,content_sha256,snapshot_json,status,created_at,request_sha256) VALUES(?,?,?,?,?,?,'queued','2026-01-01T00:00:00Z',?)")
        .bind(uuid::Uuid::new_v4().to_string()).bind(&project.id).bind(&revision.id).bind(&approval).bind(&revision.content_sha256).bind(bad_snapshot).bind("c".repeat(64)).execute(&f.pool).await;
    assert!(
        rejected.is_err(),
        "snapshot mismatch must be blocked by trigger"
    );
    assert!(
        sqlx::query("UPDATE render_jobs SET status='not_a_status' WHERE id=?")
            .bind(&queued_survives)
            .execute(&f.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE render_jobs SET status='failed' WHERE id=?")
            .bind(&queued_survives)
            .execute(&f.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE render_jobs SET status='succeeded' WHERE id=?")
            .bind(&queued_survives)
            .execute(&f.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE render_jobs SET snapshot_json='{}' WHERE id=?")
            .bind(&queued_survives)
            .execute(&f.pool)
            .await
            .is_err()
    );

    let cancel = f
        .store
        .cancel_render_job(
            &queued_cancel,
            JobCancelCommand {
                schema_version: "1".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(cancel.status, scenivox::domain::RenderJobStatus::Cancelled);
    let bad_transition = sqlx::query("UPDATE render_jobs SET status='queued' WHERE id=?")
        .bind(&queued_cancel)
        .execute(&f.pool)
        .await;
    assert!(
        bad_transition.is_err(),
        "terminal status cannot transition to queued"
    );
    let malformed_terminal = sqlx::query("UPDATE render_jobs SET status='failed' WHERE id=?")
        .bind(&queued_cancel)
        .execute(&f.pool)
        .await;
    assert!(
        malformed_terminal.is_err(),
        "failed status requires error_code"
    );

    drop(f.store.clone());
    f.pool.close().await;
    let reopened = db::connect(&f.reopen_url()).await.unwrap();
    let queued_status: String = sqlx::query_scalar("SELECT status FROM render_jobs WHERE id=?")
        .bind(&queued_cancel)
        .fetch_one(&reopened)
        .await
        .unwrap();
    assert_eq!(queued_status, "cancelled");
    let queued_survivor_status: String =
        sqlx::query_scalar("SELECT status FROM render_jobs WHERE id=?")
            .bind(&queued_survives)
            .fetch_one(&reopened)
            .await
            .unwrap();
    assert_eq!(queued_survivor_status, "queued");
    for (status, id) in &ids {
        let (got, error): (String, Option<String>) =
            sqlx::query_as("SELECT status,error_code FROM render_jobs WHERE id=?")
                .bind(id)
                .fetch_one(&reopened)
                .await
                .unwrap();
        if active.contains(status) {
            assert_eq!(
                (got.as_str(), error.as_deref()),
                ("interrupted", Some("startup_interrupted"))
            );
        } else {
            assert_eq!(got, *status, "terminal status must remain unchanged");
        }
    }
    let mut after_first_recovery = Vec::new();
    for (_, id) in &ids {
        let row = sqlx::query("SELECT status,error_code FROM render_jobs WHERE id=?")
            .bind(id)
            .fetch_one(&reopened)
            .await
            .unwrap();
        after_first_recovery.push((
            id.clone(),
            row.get::<String, _>("status"),
            row.get::<Option<String>, _>("error_code"),
        ));
    }
    reopened.close().await;
    let again = db::connect(&f.reopen_url()).await.unwrap();
    for (id, expected_status, expected_error) in after_first_recovery {
        let row = sqlx::query("SELECT status,error_code FROM render_jobs WHERE id=?")
            .bind(id)
            .fetch_one(&again)
            .await
            .unwrap();
        assert_eq!(row.get::<String, _>("status"), expected_status);
        assert_eq!(row.get::<Option<String>, _>("error_code"), expected_error);
    }
    again.close().await;
}

#[tokio::test]
async fn http_fail_closed_routes_and_cancel_schema_validation() {
    let f = Fixture::new().await;
    let (project, revision, approval) = approved(&f, None).await;
    let body = serde_json::json!({"schema_version":"1", "revision_id":revision.id, "content_sha256":revision.content_sha256, "cost_consent_id":null}).to_string();
    let enqueue_response = api::router(f.pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/projects/{}/render-jobs", project.id))
                .header("content-type", "application/json")
                .header("idempotency-key", uuid::Uuid::new_v4().to_string())
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(enqueue_response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let seeded = seed_job(&f, &project, &revision, &approval, "queued").await;
    let invalid_cancel = api::router(f.pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/render-jobs/{seeded}/cancel"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"schema_version":"2"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid_cancel.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let valid_cancel = api::router(f.pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/render-jobs/{seeded}/cancel"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"schema_version":"1"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(valid_cancel.status(), StatusCode::OK);
    let retry_response = api::router(f.pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/render-jobs/{seeded}/retry"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"schema_version":"1","cost_consent_id":null}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(retry_response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM render_jobs")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(rows, 1, "rejected enqueue/retry must not create rows");
}

#[tokio::test]
async fn cli_cancel_rejects_unsupported_schema_version() {
    let f = Fixture::new().await;
    let (project, revision, approval) = approved(&f, None).await;
    let job = seed_job(&f, &project, &revision, &approval, "cancelled").await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("cancel.json");
    std::fs::write(&file, r#"{"schema_version":"2"}"#).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args([
            "render-job",
            "cancel",
            &job,
            "--file",
            file.to_str().unwrap(),
        ])
        .env("SCENIVOX_DATABASE_URL", f.reopen_url())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["code"], "unsupported_schema_version");
    let retry_file = dir.path().join("retry.json");
    std::fs::write(
        &retry_file,
        r#"{"schema_version":"1","cost_consent_id":null}"#,
    )
    .unwrap();
    let retry_output = std::process::Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args([
            "render-job",
            "retry",
            &job,
            "--file",
            retry_file.to_str().unwrap(),
        ])
        .env("SCENIVOX_DATABASE_URL", f.reopen_url())
        .output()
        .unwrap();
    assert!(!retry_output.status.success());
    let retry_error: serde_json::Value = serde_json::from_slice(&retry_output.stderr).unwrap();
    assert_eq!(retry_error["error"]["code"], "skill_unavailable");
}
