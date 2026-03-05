//! HTTP server for daemon mode.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use apiary::{Pool, PoolError, Task};

#[derive(Clone)]
struct AppState {
    pool: Pool,
    api_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExecuteTaskRequest {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    working_dir: Option<PathBuf>,
    #[serde(default)]
    env: HashMap<String, String>,
    session_id: String,
}

#[derive(Debug, Serialize)]
struct ExecuteTaskResponse {
    task_id: String,
    session_id: String,
    exit_code: i32,
    timed_out: bool,
    duration_ms: u128,
    success: bool,
    stdout: String,
    stderr: String,
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    total: usize,
    idle: usize,
    busy: usize,
    reserved: usize,
    error: usize,
    min_sandboxes: usize,
    max_sandboxes: usize,
    tasks_executed: u64,
    tasks_succeeded: u64,
    tasks_failed: u64,
    avg_task_duration_ms: u64,
}

#[derive(Debug, Serialize)]
struct CreateSessionResponse {
    session_id: String,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

pub async fn run_server(
    bind: String,
    pool: Pool,
    api_token: Option<String>,
) -> anyhow::Result<()> {
    let state = AppState {
        pool,
        api_token: api_token.clone(),
    };

    let api_routes = Router::new()
        .route("/api/v1/status", get(status))
        .route("/api/v1/tasks", post(execute_task))
        .route("/api/v1/sessions", post(create_session))
        .route("/api/v1/sessions/:session_id", delete(close_session))
        .layer(middleware::from_fn_with_state(state.clone(), auth_layer));

    let app = Router::new()
        .route("/healthz", get(healthz))
        .merge(api_routes)
        .with_state(state);

    if api_token.is_some() {
        tracing::info!("API authentication enabled (Bearer token required)");
    }

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    let local_addr = listener.local_addr()?;
    tracing::info!("API server listening on http://{local_addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn auth_layer(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: middleware::Next,
) -> Result<Response, StatusCode> {
    if let Some(ref expected) = state.api_token {
        let auth = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "));

        match auth {
            Some(token) if token == expected.as_str() => {}
            _ => {
                return Ok((
                    StatusCode::UNAUTHORIZED,
                    Json(ErrorResponse {
                        error: "invalid or missing Bearer token".to_string(),
                    }),
                )
                    .into_response())
            }
        }
    }
    Ok(next.run(request).await)
}

async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_ok() {
        tracing::info!("Shutdown signal received");
    }
}

async fn healthz() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn status(State(state): State<AppState>) -> Json<StatusResponse> {
    let status = state.pool.status();
    Json(StatusResponse {
        total: status.total,
        idle: status.idle,
        busy: status.busy,
        reserved: status.reserved,
        error: status.error,
        min_sandboxes: status.min_sandboxes,
        max_sandboxes: status.max_sandboxes,
        tasks_executed: status.tasks_executed,
        tasks_succeeded: status.tasks_succeeded,
        tasks_failed: status.tasks_failed,
        avg_task_duration_ms: status.avg_task_duration_ms,
    })
}

async fn create_session(
    State(state): State<AppState>,
) -> Result<Json<CreateSessionResponse>, ApiError> {
    let session_id = state
        .pool
        .create_session()
        .await
        .map_err(ApiError::from_pool_error)?;

    Ok(Json(CreateSessionResponse { session_id }))
}

async fn close_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state
        .pool
        .close_session(&session_id)
        .await
        .map_err(ApiError::from_pool_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn execute_task(
    State(state): State<AppState>,
    Json(payload): Json<ExecuteTaskRequest>,
) -> Result<Response, ApiError> {
    let session_id = payload.session_id.trim().to_string();
    if session_id.is_empty() {
        return Err(ApiError::bad_request("session_id must not be empty"));
    }
    let task = build_task(payload, &state.pool)?;

    let result = state
        .pool
        .execute_in_session(&session_id, task)
        .await
        .map_err(ApiError::from_pool_error)?;
    let success = result.success();
    let stdout = result.stdout_lossy();
    let stderr = result.stderr_lossy();

    Ok(Json(ExecuteTaskResponse {
        task_id: result.task_id,
        session_id,
        exit_code: result.exit_code,
        timed_out: result.timed_out,
        duration_ms: result.duration.as_millis(),
        success,
        stdout,
        stderr,
    })
    .into_response())
}

fn build_task(payload: ExecuteTaskRequest, pool: &Pool) -> Result<Task, ApiError> {
    let command = payload.command.trim();
    if command.is_empty() {
        return Err(ApiError::bad_request("command must not be empty"));
    }

    let timeout = if let Some(ms) = payload.timeout_ms {
        Duration::from_millis(ms)
    } else if let Some(secs) = payload.timeout_secs {
        Duration::from_secs(secs)
    } else {
        pool.config().default_timeout
    };

    let mut env = pool.config().default_env.clone();
    env.extend(payload.env);

    let working_dir = payload
        .working_dir
        .unwrap_or_else(|| pool.config().default_workdir.clone());
    let task = Task::new(command).timeout(timeout).envs(env).working_dir(working_dir);

    Ok(task)
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }

    fn from_pool_error(error: PoolError) -> Self {
        match error {
            PoolError::NoIdleSandbox(_) => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: error.to_string(),
            },
            PoolError::ShuttingDown => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: "pool is shutting down".to_string(),
            },
            PoolError::SessionNotFound(session_id) => Self {
                status: StatusCode::NOT_FOUND,
                message: format!("session not found: {session_id}"),
            },
            other => Self::internal(other.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response()
    }
}
