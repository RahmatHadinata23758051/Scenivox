use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use scenivox::{api, db};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn request(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    key: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(key) = key {
        request = request.header("Idempotency-Key", key);
    }
    let response = app
        .oneshot(
            request
                .body(Body::from(body.map(|v| v.to_string()).unwrap_or_default()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        },
    )
}

#[tokio::test]
async fn http_crud_revisions_replay_and_stale_writes_share_persistence() {
    let temp = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        temp.path()
            .join("http.db")
            .to_string_lossy()
            .replace('\\', "/")
    );
    let pool = db::connect(&url).await.unwrap();
    let app = api::router(pool.clone());
    let project: Value = serde_json::from_str(include_str!("fixtures/p1-project.json")).unwrap();
    let key = "00000000-0000-4000-8000-000000000001";
    let (status, created) = request(
        app.clone(),
        "POST",
        "/v1/projects",
        Some(project.clone()),
        Some(key),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let (status, replayed) = request(
        app.clone(),
        "POST",
        "/v1/projects",
        Some(project.clone()),
        Some(key),
    )
    .await;
    assert!(status.is_success());
    assert_eq!(created, replayed);
    let mut conflict = project.clone();
    conflict["title"] = json!("Different request");
    let (status, error) = request(
        app.clone(),
        "POST",
        "/v1/projects",
        Some(conflict),
        Some(key),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["error"]["code"], "idempotency_conflict");
    let id = created["id"].as_str().unwrap();
    let uri = format!("/v1/projects/{id}");
    assert_eq!(
        request(app.clone(), "GET", &uri, None, None).await.1,
        created
    );
    let revision_uri = format!("{uri}/revisions");
    let draft: Value = serde_json::from_str(include_str!("fixtures/p1-revision.json")).unwrap();
    let (status, first) = request(
        app.clone(),
        "POST",
        &revision_uri,
        Some(draft.clone()),
        Some("00000000-0000-4000-8000-000000000002"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    let (status, error) = request(
        app.clone(),
        "POST",
        &revision_uri,
        Some(draft),
        Some("00000000-0000-4000-8000-000000000003"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["error"]["code"], "stale_revision");
    let (status, history) = request(app.clone(), "GET", &revision_uri, None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(history["items"], json!([first]));
    let (status, updated) = request(
        app.clone(),
        "PATCH",
        &uri,
        Some(json!({"schema_version":"1","title":"Renamed"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["title"], "Renamed");
    let (status, _) = request(app.clone(), "DELETE", &uri, None, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        request(app.clone(), "GET", &uri, None, None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(app, "GET", &revision_uri, None, None).await.0,
        StatusCode::NOT_FOUND
    );
    pool.close().await;
}

#[tokio::test]
async fn http_rejects_duplicate_keys_unknown_fields_and_missing_replay_key() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let app = api::router(pool.clone());
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/projects")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"schema_version":"1","schema_version":"2"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let project: Value = serde_json::from_str(include_str!("fixtures/p1-project.json")).unwrap();
    let (status, error) = request(
        app.clone(),
        "POST",
        "/v1/projects",
        Some(project.clone()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["error"]["code"], "missing_idempotency_key");
    let mut unknown = project;
    unknown["approved_revision_id"] = json!("invented");
    assert_eq!(
        request(app, "POST", "/v1/projects", Some(unknown), None)
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    pool.close().await;
}

#[tokio::test]
async fn http_model_registry_is_local_and_resolves_explicit_role_pairs() {
    let pool = db::connect("sqlite::memory:").await.unwrap();
    let app = api::router(pool.clone());
    let (status, models) = request(app.clone(), "GET", "/v1/models", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(models["schema_version"], "1");
    assert_eq!(models["items"].as_array().unwrap().len(), 2);
    assert_eq!(models["next_cursor"], Value::Null);
    let (status, defaults) = request(app.clone(), "GET", "/v1/settings/models", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(defaults["schema_version"], "1");
    assert_eq!(
        defaults["model_selection"]["storyboard_planning"]["model_id"],
        "planner-v1"
    );
    let (status, selected) = request(
        app.clone(),
        "POST",
        "/v1/model-selections:resolve",
        Some(json!({
            "schema_version":"1", "model_selection":{}
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{selected}");
    assert_eq!(
        selected["model_selection"]["storyboard_planning"]["model_id"],
        "planner-v1"
    );
    assert_eq!(
        selected["model_selection"]["scene_code_generation"]["model_id"],
        "coder-v1"
    );
    let (status, registered) = request(
        app.clone(),
        "PUT",
        "/v1/models/mock/planner-alt",
        Some(json!({
            "schema_version":"1", "provider_id":"mock", "model_id":"planner-alt",
            "adapter_version":"1", "task_roles":["storyboard_planning"],
            "parameters":{"temperature":0,"seed":0}, "enabled":true,"configured":true,
            "availability":"available","reason":"local test registration","remote":false
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{registered}");
    let mut model_schema: Value =
        serde_json::from_str(include_str!("../schemas/v1/contracts.schema.json")).unwrap();
    model_schema.as_object_mut().unwrap().remove("oneOf");
    model_schema["$ref"] = json!("#/$defs/Model");
    let model_validator = jsonschema::validator_for(&model_schema).unwrap();
    assert!(model_validator.is_valid(&registered));
    let (status, models) = request(app.clone(), "GET", "/v1/models", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(models["items"].as_array().unwrap().len(), 3);
    assert!(
        models["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| model_validator.is_valid(m))
    );
    let (status, duplicate) = request(app.clone(), "PUT", "/v1/models/mock/duplicate", Some(json!({
        "schema_version":"1", "provider_id":"mock", "model_id":"duplicate", "adapter_version":"1",
        "task_roles":["storyboard_planning","storyboard_planning"], "parameters":{"temperature":0},
        "enabled":true,"configured":true,"availability":"available","reason":"","remote":false
    })), None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(duplicate["error"]["code"], "unsupported_capability");
    let (status, unverified_remote) = request(app.clone(), "PUT", "/v1/models/remote/model-v1", Some(json!({
        "schema_version":"1", "provider_id":"remote", "model_id":"model-v1", "adapter_version":"1",
        "task_roles":["storyboard_planning"], "parameters":{"temperature":0},
        "enabled":true,"configured":true,"availability":"available","reason":"unverified","remote":true
    })), None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        unverified_remote["error"]["code"],
        "invalid_model_selection"
    );
    let (status, settings) = request(
        app.clone(),
        "PUT",
        "/v1/settings/models",
        Some(json!({
            "schema_version":"1", "model_selection":{
                "storyboard_planning":{"provider_id":"mock","model_id":"planner-v1"},
                "scene_code_generation":{"provider_id":"mock","model_id":"coder-v1"}
            }
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{settings}");
    assert_eq!(settings["schema_version"], "1");
    assert_eq!(
        settings["model_selection"]["storyboard_planning"]["model_id"],
        "planner-v1"
    );
    let (status, project) = request(app.clone(), "POST", "/v1/projects", Some(json!({
        "schema_version":"1", "title":"Model settings test", "idea":"Test selection updates",
        "target":{"duration_ms":1000,"aspect_ratio":"9:16","fps":30,"language":"id-ID","style_id":"stick-figure"},
        "model_selection":{}
    })), Some("00000000-0000-4000-8000-000000000021")).await;
    assert_eq!(status, StatusCode::CREATED, "{project}");
    let project_path = format!(
        "/v1/projects/{}/model-settings",
        project["id"].as_str().unwrap()
    );
    let selection_update = json!({"schema_version":"1","model_selection":{"storyboard_planning":{"provider_id":"mock","model_id":"planner-alt"}}});
    let (status, patched) = request(
        app.clone(),
        "PATCH",
        &project_path,
        Some(selection_update.clone()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(
        patched["model_selection"]["storyboard_planning"]["model_id"],
        "planner-alt"
    );
    let (status, legacy_put) = request(app.clone(), "PUT", &project_path, Some(json!({
        "schema_version":"1","model_selection":{"scene_code_generation":{"provider_id":"mock","model_id":"coder-v1"}}
    })), None).await;
    assert_eq!(status, StatusCode::OK, "{legacy_put}");
    assert_eq!(
        legacy_put["model_selection"]["scene_code_generation"]["model_id"],
        "coder-v1"
    );
    let (status, resolved) = request(
        app,
        "POST",
        "/v1/model-selections:resolve",
        Some(json!({
            "schema_version":"1", "project_id":null,
            "model_selection":{"storyboard_planning":null,"scene_code_generation":null}
        })),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "null is not a sparse omission"
    );
    assert_eq!(resolved["error"]["code"], "invalid_schema");
    pool.close().await;
}

#[test]
fn cli_creates_and_reads_same_project_and_revision_after_process_exit() {
    let temp = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        temp.path()
            .join("cli.db")
            .to_string_lossy()
            .replace('\\', "/")
    );
    let project_file = temp.path().join("project.json");
    let revision_file = temp.path().join("revision.json");
    std::fs::write(&project_file, include_str!("fixtures/p1-project.json")).unwrap();
    std::fs::write(&revision_file, include_str!("fixtures/p1-revision.json")).unwrap();
    let run = |args: Vec<&str>| {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_scenivox"))
            .args(args)
            .current_dir(temp.path())
            .env("SCENIVOX_DATABASE_URL", &url)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let project = run(vec![
        "project",
        "create",
        "--file",
        project_file.to_str().unwrap(),
    ]);
    let id = project["id"].as_str().unwrap();
    assert_eq!(run(vec!["project", "get", id]), project);
    let revision = run(vec![
        "revision",
        "create",
        id,
        "--file",
        revision_file.to_str().unwrap(),
    ]);
    let rid = revision["id"].as_str().unwrap();
    assert_eq!(run(vec!["revision", "get", id, rid]), revision);
    let models = run(vec!["model", "list"]);
    assert_eq!(models.as_array().unwrap().len(), 2);
    let defaults = run(vec!["model", "settings-get"]);
    assert_eq!(defaults["storyboard_planning"]["model_id"], "planner-v1");
}

#[tokio::test]
async fn render_job_cli_fails_closed_without_renderer() {
    let temp = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        temp.path()
            .join("job.db")
            .to_string_lossy()
            .replace('\\', "/")
    );
    let project_file = temp.path().join("project.json");
    std::fs::write(&project_file, include_str!("fixtures/p1-project.json")).unwrap();
    let create = std::process::Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args([
            "project",
            "create",
            "--file",
            project_file.to_str().unwrap(),
        ])
        .env("SCENIVOX_DATABASE_URL", &url)
        .output()
        .unwrap();
    assert!(create.status.success());
    let project: Value = serde_json::from_slice(&create.stdout).unwrap();
    let enqueue = temp.path().join("enqueue.json");
    std::fs::write(
        &enqueue,
        serde_json::json!({
            "schema_version":"1", "revision_id":"00000000-0000-4000-8000-000000000001",
            "content_sha256":"a".repeat(64), "cost_consent_id":null
        })
        .to_string(),
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_scenivox"))
        .args([
            "render-job",
            "enqueue",
            project["id"].as_str().unwrap(),
            "--file",
            enqueue.to_str().unwrap(),
        ])
        .env("SCENIVOX_DATABASE_URL", &url)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["code"], "stale_revision");
}

#[tokio::test]
async fn cli_review_approval_survives_process_exit_and_database_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        temp.path()
            .join("review-cli.db")
            .to_string_lossy()
            .replace('\\', "/")
    );
    let project_file = temp.path().join("project.json");
    let revision_file = temp.path().join("revision.json");
    std::fs::write(&project_file, include_str!("fixtures/p1-project.json")).unwrap();
    std::fs::write(&revision_file, include_str!("fixtures/p1-revision.json")).unwrap();
    let run = |args: Vec<String>| {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_scenivox"))
            .args(args)
            .current_dir(temp.path())
            .env("SCENIVOX_DATABASE_URL", &url)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let project = run(vec![
        "project".into(),
        "create".into(),
        "--file".into(),
        project_file.to_string_lossy().into_owned(),
    ]);
    let project_id = project["id"].as_str().unwrap().to_owned();
    let revision = run(vec![
        "revision".into(),
        "create".into(),
        project_id.clone(),
        "--file".into(),
        revision_file.to_string_lossy().into_owned(),
    ]);
    let revision_id = revision["id"].as_str().unwrap().to_owned();
    let review_file = temp.path().join("review.json");
    std::fs::write(
        &review_file,
        serde_json::json!({
            "schema_version":"1", "content_sha256":revision["content_sha256"]
        })
        .to_string(),
    )
    .unwrap();
    let review = run(vec![
        "revision".into(),
        "review".into(),
        project_id.clone(),
        revision_id.clone(),
        "--file".into(),
        review_file.to_string_lossy().into_owned(),
    ]);
    assert_eq!(review["review_state"], "in_review");
    let approval = run(vec![
        "revision".into(),
        "approve".into(),
        project_id.clone(),
        revision_id.clone(),
        "--file".into(),
        review_file.to_string_lossy().into_owned(),
    ]);
    assert_eq!(approval["project_id"], project_id);
    assert_eq!(approval["revision_id"], revision_id);

    let reopened = db::connect(&url).await.unwrap();
    let store = scenivox::store::Store::new(reopened.clone());
    let reopened_project = store.get_project(&project_id).await.unwrap();
    assert_eq!(
        reopened_project.approved_revision_id.as_deref(),
        Some(revision_id.as_str())
    );
    let persisted_approval = store
        .get_approval(&project_id, approval["id"].as_str().unwrap())
        .await
        .unwrap();
    assert_eq!(serde_json::to_value(&persisted_approval).unwrap(), approval);
    let review_state: String = sqlx::query_scalar(
        "SELECT review_state FROM revision_reviews WHERE project_id=? AND revision_id=?",
    )
    .bind(&project_id)
    .bind(&revision_id)
    .fetch_one(&reopened)
    .await
    .unwrap();
    assert_eq!(review_state, "approved");
    reopened.close().await;
}
