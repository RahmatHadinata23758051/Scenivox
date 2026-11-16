use crate::{
    domain::{CreateProject, CreateRevision, ListQuery, UpdateProject},
    json as command_json,
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

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

pub fn router(db: SqlitePool) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/projects", get(list_projects).post(create_project))
        .route(
            "/v1/projects/{id}",
            get(get_project)
                .patch(update_project)
                .delete(delete_project),
        )
        .route(
            "/v1/projects/{id}/revisions",
            get(list_revisions).post(create_revision),
        )
        .route(
            "/v1/projects/{id}/revisions/{rid}",
            get(get_revision).patch(edit_revision),
        )
        .with_state(AppState { db })
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
            StoreError::StaleRevision | StoreError::IdempotencyConflict => StatusCode::CONFLICT,
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
