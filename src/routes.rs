use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;

use crate::state::AppState;

pub fn health_router() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/kb-api/v1/healthz", get(healthz))
        .route("/kb-api/v1/readyz", get(readyz))
        .route("/kb-api/v1/knowledge/context", get(crate::catalog::context))
        .route("/kb-api/v1/admin/catalog/sync", post(crate::catalog::sync))
}

#[derive(Debug, Serialize)]
struct LivenessResponse {
    status: &'static str,
    service: &'static str,
}

async fn healthz() -> impl IntoResponse {
    Json(LivenessResponse {
        status: "ok",
        service: "course-kb-server",
    })
}

#[derive(Debug, Serialize)]
struct ReadinessResponse {
    status: &'static str,
    sqlite: DependencyStatus,
    qdrant: DependencyStatus,
}

#[derive(Debug, Serialize)]
struct DependencyStatus {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    let sqlite = match sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&state.db)
        .await
    {
        Ok(_) => DependencyStatus {
            ok: true,
            error: None,
        },
        Err(_) => DependencyStatus {
            ok: false,
            error: Some("sqlite_unavailable".to_owned()),
        },
    };
    let qdrant_health = state.qdrant.health().await;
    let qdrant = DependencyStatus {
        ok: qdrant_health.ok,
        error: (!qdrant_health.ok).then(|| "qdrant_unavailable".to_owned()),
    };
    let ready = sqlite.ok && qdrant.ok;
    let response = ReadinessResponse {
        status: if ready { "ok" } else { "unavailable" },
        sqlite,
        qdrant,
    };
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(response))
}
