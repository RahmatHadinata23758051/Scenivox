use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;
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
