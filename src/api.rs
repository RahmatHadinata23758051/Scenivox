use crate::{
    config::Config,
    domain::{
        CreateProject, CreateRenderJob, CreateRevision, JobCancelCommand, ListQuery,
        PlanStoryboard, RegisterModel, ResolveModelSelection, RetryRenderJob, ReviewCommand,
        SetModelSettings, UpdateProject,
    },
    json as command_json,
    skill::ImportSkill,
    store::{Store, StoreError},
};
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{FromRequest, Path, Query, Request, State, rejection::QueryRejection},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use sqlx::SqlitePool;
use std::path::PathBuf;

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub skill_store_dir: PathBuf,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

pub fn router(db: SqlitePool) -> Router {
    let skill_store_dir = Config::from_env()
        .map(|cfg| cfg.skill_store_dir)
        .unwrap_or_else(|_| PathBuf::from("skills"));
    router_with_skill_store(db, skill_store_dir)
}

pub fn router_with_skill_store(db: SqlitePool, skill_store_dir: PathBuf) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/skills", get(list_skills))
        .route("/v1/skills:import", axum::routing::post(import_skill))
        .route("/v1/skills/{id}/{version}", get(get_skill))
        .route("/v1/models", get(list_models))
        .route(
            "/v1/models/{provider}/{model}",
            axum::routing::put(register_model),
        )
        .route(
            "/v1/settings/models",
            get(get_model_settings).put(set_model_settings),
        )
        .route(
            "/v1/model-selections:resolve",
            axum::routing::post(resolve_model_selection),
        )
        .route(
            "/v1/projects/{id}/model-settings",
            axum::routing::patch(set_project_model_selection).put(set_project_model_selection),
        )
        .route("/v1/projects", get(list_projects).post(create_project))
        .route(
            "/v1/projects/{id}",
            get(get_project)
                .patch(update_project)
                .delete(delete_project),
        )
        .route(
            "/v1/projects/{id}/storyboard:generate",
            axum::routing::post(generate_storyboard),
        )
        .route(
            "/v1/projects/{id}/revisions",
            get(list_revisions).post(create_revision),
        )
        .route(
            "/v1/projects/{id}/revisions/{rid}",
            get(get_revision).patch(edit_revision),
        )
        .route(
            "/v1/projects/{id}/revisions/{rid}/review",
            axum::routing::post(review_revision),
        )
        .route(
            "/v1/projects/{id}/revisions/{rid}/withdraw-review",
            axum::routing::post(withdraw_review),
        )
        .route(
            "/v1/projects/{id}/revisions/{rid}/approve",
            axum::routing::post(approve_revision),
        )
        .route("/v1/projects/{id}/approvals/{aid}", get(get_approval))
        .route(
            "/v1/projects/{id}/render-jobs",
            axum::routing::post(enqueue_render_job),
        )
        .route("/v1/render-jobs/{id}", get(get_render_job))
        .route(
            "/v1/render-jobs/{id}/retry",
            axum::routing::post(retry_render_job),
        )
        .route(
            "/v1/render-jobs/{id}/cancel",
            axum::routing::post(cancel_render_job),
        )
        .with_state(AppState {
            db,
            skill_store_dir,
        })
}

async fn import_skill(
    State(state): State<AppState>,
    headers: HeaderMap,
    Command(input): Command<ImportSkill>,
) -> Result<Response, ApiError> {
    if input.schema_version != "1" {
        return Err(StoreError::Validation {
            code: "unsupported_schema_version",
            path: "/schema_version".into(),
        }
        .into());
    }
    let (skill, replay) = Store::new(state.db)
        .import_skill(
            &state.skill_store_dir,
            Some(key(&headers)?),
            std::path::Path::new(&input.source_path),
        )
        .await?;
    Ok(created(
        format!("/v1/skills/{}/{}", skill.skill_id, skill.skill_version),
        skill,
        replay,
    ))
}

async fn list_skills(
    State(state): State<AppState>,
    q: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    Ok(Json(Store::new(state.db).list_skills(query(q)?).await?).into_response())
}

async fn get_skill(
    State(state): State<AppState>,
    Path((id, version)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    Ok(Json(Store::new(state.db).get_skill(&id, &version).await?).into_response())
}

async fn list_models(State(state): State<AppState>) -> Result<Response, ApiError> {
    let items = Store::new(state.db).list_models().await?;
    Ok(
        Json(serde_json::json!({"schema_version":"1", "items":items, "next_cursor":null}))
            .into_response(),
    )
}

async fn get_model_settings(State(state): State<AppState>) -> Result<Response, ApiError> {
    let model_selection = Store::new(state.db).get_model_settings().await?;
    Ok(
        Json(serde_json::json!({"schema_version":"1", "model_selection":model_selection}))
            .into_response(),
    )
}

async fn set_model_settings(
    State(state): State<AppState>,
    Command(input): Command<SetModelSettings>,
) -> Result<Response, ApiError> {
    let model_selection = Store::new(state.db).set_model_settings(input).await?;
    Ok(
        Json(serde_json::json!({"schema_version":"1", "model_selection":model_selection}))
            .into_response(),
    )
}

async fn set_project_model_selection(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Command(input): Command<SetModelSettings>,
) -> Result<Response, ApiError> {
    Ok(Json(
        Store::new(state.db)
            .set_project_model_selection(&id, input)
            .await?,
    )
    .into_response())
}

async fn resolve_model_selection(
    State(state): State<AppState>,
    Command(input): Command<ResolveModelSelection>,
) -> Result<Response, ApiError> {
    Ok(Json(Store::new(state.db).resolve_model_selection(input).await?).into_response())
}

async fn generate_storyboard(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
    Command(input): Command<PlanStoryboard>,
) -> Result<Response, ApiError> {
    let revision = Store::new(state.db)
        .plan_storyboard(
            &project_id,
            Some(key(&headers)?),
            input,
            &crate::planner::DeterministicMockPlanner,
        )
        .await?;
    Ok(created(
        format!("/v1/projects/{project_id}/revisions/{}", revision.id),
        revision,
        false,
    ))
}

async fn register_model(
    State(state): State<AppState>,
    Path((provider, model)): Path<(String, String)>,
    Command(input): Command<RegisterModel>,
) -> Result<Response, ApiError> {
    if input.provider_id != provider || input.model_id != model {
        return Err(StoreError::Validation {
            code: "invalid_model_selection",
            path: "/model_selection".into(),
        }
        .into());
    }
    Ok(Json(Store::new(state.db).register_model(input).await?).into_response())
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let status = match sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&state.db)
        .await
    {
        Ok(1) => "ok",
        _ => "unavailable",
    };
    Json(HealthResponse { status })
}

struct ApiError {
    status: StatusCode,
    code: String,
    path: Option<String>,
}
impl ApiError {
    fn new(status: StatusCode, code: &str) -> Self {
        Self {
            status,
            code: code.into(),
            path: None,
        }
    }
}
impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        let status = match &error {
            StoreError::NotFound => StatusCode::NOT_FOUND,
            StoreError::Conflict {
                code: "skill_unavailable",
            } => StatusCode::SERVICE_UNAVAILABLE,
            StoreError::StaleRevision
            | StoreError::IdempotencyConflict
            | StoreError::Conflict { .. } => StatusCode::CONFLICT,
            StoreError::Validation { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            StoreError::Database(_) => StatusCode::SERVICE_UNAVAILABLE,
            StoreError::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let mut result = Self::new(status, error.code());
        if let StoreError::Validation { path, .. } = error {
            result.path = Some(path);
        }
        result
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let details = self
            .path
            .map(|path| vec![json!({"path":path,"code":self.code})])
            .unwrap_or_default();
        (self.status, Json(json!({"schema_version":"1","error":{"code":self.code,"message":self.status.canonical_reason().unwrap_or("Request failed"),"details":details}}))).into_response()
    }
}

struct Command<T>(T);
impl<S, T> FromRequest<S> for Command<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;
    async fn from_request(request: Request, _: &S) -> Result<Self, Self::Rejection> {
        let media = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if media.split(';').next().map(str::trim) != Some("application/json") {
            return Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
            ));
        }
        let body = to_bytes(request.into_body(), command_json::MAX_BODY_BYTES)
            .await
            .map_err(|_| ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"))?;
        let value = command_json::parse_value(&body)
            .map_err(|code| ApiError::new(StatusCode::BAD_REQUEST, &code))?;
        if value
            .get("schema_version")
            .and_then(|v| v.as_str())
            .is_some_and(|v| v != "1")
        {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unsupported_schema_version",
            ));
        }
        serde_json::from_value(value)
            .map(Self)
            .map_err(|_| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_schema"))
    }
}

fn key(headers: &HeaderMap) -> Result<&str, ApiError> {
    let key = headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "missing_idempotency_key"))?;
    if !uuid::Uuid::parse_str(key).is_ok_and(|id| id.to_string() == key) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "missing_idempotency_key",
        ));
    }
    Ok(key)
}
fn query(value: Result<Query<ListQuery>, QueryRejection>) -> Result<ListQuery, ApiError> {
    value
        .map(|Query(q)| q)
        .map_err(|_| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_cursor"))
}
fn created<T: Serialize>(location: String, value: T, _replay: bool) -> Response {
    let mut response = (StatusCode::CREATED, Json(value)).into_response();
    if let Ok(value) = location.parse() {
        response.headers_mut().insert(header::LOCATION, value);
    }
    response
}
async fn create_project(
    State(state): State<AppState>,
    headers: HeaderMap,
    Command(input): Command<CreateProject>,
) -> Result<Response, ApiError> {
    let (project, replay) = Store::new(state.db)
        .create_project_idempotent(key(&headers)?, input)
        .await?;
    Ok(created(
        format!("/v1/projects/{}", project.id),
        project,
        replay,
    ))
}
async fn get_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    Ok(Json(Store::new(state.db).get_project(&id).await?).into_response())
}
async fn list_projects(
    State(state): State<AppState>,
    q: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    Ok(Json(Store::new(state.db).list_projects(query(q)?).await?).into_response())
}
async fn update_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Command(input): Command<UpdateProject>,
) -> Result<Response, ApiError> {
    Ok(Json(Store::new(state.db).update_project(&id, input).await?).into_response())
}
async fn delete_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    Store::new(state.db).delete_project(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn create_revision(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Command(input): Command<CreateRevision>,
) -> Result<Response, ApiError> {
    let (revision, replay) = Store::new(state.db)
        .create_revision_idempotent(&id, key(&headers)?, input)
        .await?;
    Ok(created(
        format!("/v1/projects/{id}/revisions/{}", revision.id),
        revision,
        replay,
    ))
}
async fn edit_revision(
    State(state): State<AppState>,
    Path((id, rid)): Path<(String, String)>,
    Command(input): Command<CreateRevision>,
) -> Result<Response, ApiError> {
    Store::new(state.db.clone()).get_revision(&id, &rid).await?;
    if input.expected_revision_id.as_deref() != Some(&rid) {
        return Err(StoreError::StaleRevision.into());
    }
    let revision = Store::new(state.db).create_revision(&id, input).await?;
    Ok(created(
        format!("/v1/projects/{id}/revisions/{}", revision.id),
        revision,
        false,
    ))
}
async fn get_revision(
    State(state): State<AppState>,
    Path((id, rid)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    Ok(Json(Store::new(state.db).get_revision(&id, &rid).await?).into_response())
}
async fn list_revisions(
    State(state): State<AppState>,
    Path(id): Path<String>,
    q: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    Ok(Json(Store::new(state.db).list_revisions(&id, query(q)?).await?).into_response())
}
async fn review_revision(
    State(state): State<AppState>,
    Path((id, rid)): Path<(String, String)>,
    headers: HeaderMap,
    Command(input): Command<ReviewCommand>,
) -> Result<Response, ApiError> {
    let review = Store::new(state.db)
        .review_revision(&id, &rid, Some(key(&headers)?), input)
        .await?;
    Ok(Json(review).into_response())
}

async fn withdraw_review(
    State(state): State<AppState>,
    Path((id, rid)): Path<(String, String)>,
    headers: HeaderMap,
    Command(input): Command<ReviewCommand>,
) -> Result<Response, ApiError> {
    let review = Store::new(state.db)
        .withdraw_review(&id, &rid, Some(key(&headers)?), input)
        .await?;
    Ok(Json(review).into_response())
}

async fn approve_revision(
    State(state): State<AppState>,
    Path((id, rid)): Path<(String, String)>,
    headers: HeaderMap,
    Command(input): Command<ReviewCommand>,
) -> Result<Response, ApiError> {
    let (approval, replay, duplicate) = Store::new(state.db)
        .approve_revision(&id, &rid, Some(key(&headers)?), input)
        .await?;
    let status = if duplicate && !replay {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    let location = format!("/v1/projects/{id}/approvals/{}", approval.id);
    let mut response = (status, Json(approval)).into_response();
    if let Ok(location) = location.parse() {
        response.headers_mut().insert(header::LOCATION, location);
    }
    Ok(response)
}
async fn get_approval(
    State(state): State<AppState>,
    Path((id, aid)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    Ok(Json(Store::new(state.db).get_approval(&id, &aid).await?).into_response())
}
async fn enqueue_render_job(
    State(state): State<AppState>,
    Path(pid): Path<String>,
    headers: HeaderMap,
    Command(input): Command<CreateRenderJob>,
) -> Result<Response, ApiError> {
    let (job, replay) = Store::new(state.db)
        .enqueue_render_job(&pid, Some(key(&headers)?), input)
        .await?;
    Ok(created(format!("/v1/render-jobs/{}", job.id), job, replay))
}

async fn get_render_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    Ok(Json(Store::new(state.db).get_render_job(&id).await?).into_response())
}

async fn retry_render_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Command(input): Command<RetryRenderJob>,
) -> Result<Response, ApiError> {
    let job = Store::new(state.db).retry_render_job(&id, input).await?;
    Ok(created(format!("/v1/render-jobs/{}", job.id), job, false))
}

async fn cancel_render_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Command(input): Command<JobCancelCommand>,
) -> Result<Response, ApiError> {
    Ok(Json(Store::new(state.db).cancel_render_job(&id, input).await?).into_response())
}
