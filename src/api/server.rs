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

use apiary::{Pool, PoolConfig, PoolError, PoolStatus, SessionOptions, Task};

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

#[derive(Debug, Default, Deserialize)]
struct CreateSessionRequest {
    #[serde(default)]
    working_dir: Option<PathBuf>,
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
struct CreateSessionResponse {
    session_id: String,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

pub async fn run_server(bind: String, pool: Pool, api_token: Option<String>) -> anyhow::Result<()> {
    let state = AppState {
        pool,
        api_token: api_token.clone(),
    };

    let api_routes = Router::new()
        .route("/api/v1/status", get(status))
        .route("/api/v1/tasks", post(execute_task))
        .route("/api/v1/sessions", post(create_session))
        .route("/api/v1/sessions/{session_id}", delete(close_session))
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

/// Constant-time byte comparison to prevent timing attacks on token validation.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
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
            Some(token) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => {}
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

async fn status(State(state): State<AppState>) -> Json<PoolStatus> {
    Json(state.pool.status())
}

async fn create_session(
    State(state): State<AppState>,
    payload: Option<Json<CreateSessionRequest>>,
) -> Result<Json<CreateSessionResponse>, ApiError> {
    let payload = payload.map(|Json(payload)| payload).unwrap_or_default();
    let session_options = payload
        .working_dir
        .map(|working_dir| SessionOptions::default().working_dir(working_dir))
        .unwrap_or_default();
    let session_id = state
        .pool
        .create_session(session_options)
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
    let task = build_task(payload, state.pool.config())?;

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

fn build_task(payload: ExecuteTaskRequest, config: &PoolConfig) -> Result<Task, ApiError> {
    let command = payload.command.trim();
    if command.is_empty() {
        return Err(ApiError::bad_request("command must not be empty"));
    }

    let timeout = if let Some(ms) = payload.timeout_ms {
        Duration::from_millis(ms)
    } else if let Some(secs) = payload.timeout_secs {
        Duration::from_secs(secs)
    } else {
        config.default_timeout
    };

    let mut env = config.default_env.clone();
    env.extend(payload.env);

    let mut task = Task::new(command).timeout(timeout).envs(env);
    if let Some(working_dir) = payload.working_dir {
        task = task.working_dir(working_dir);
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> PoolConfig {
        PoolConfig::builder()
            .base_image("/tmp/rootfs")
            .default_timeout(Duration::from_secs(42))
            .default_workdir("/workspace/default")
            .env("DEFAULT_KEY", "default-value")
            .build()
            .expect("config should build")
    }

    #[test]
    fn build_task_leaves_working_dir_unset_without_override() {
        let task = build_task(
            ExecuteTaskRequest {
                command: "echo hello".to_string(),
                timeout_ms: None,
                timeout_secs: None,
                working_dir: None,
                env: HashMap::new(),
                session_id: "session-1".to_string(),
            },
            &test_config(),
        )
        .expect("task should build");

        assert_eq!(task.working_dir, None);
        assert_eq!(task.timeout, Duration::from_secs(42));
        assert_eq!(
            task.env.get("DEFAULT_KEY"),
            Some(&"default-value".to_string())
        );
    }

    #[test]
    fn build_task_preserves_explicit_working_dir_override() {
        let task = build_task(
            ExecuteTaskRequest {
                command: "echo hello".to_string(),
                timeout_ms: None,
                timeout_secs: None,
                working_dir: Some(PathBuf::from("src")),
                env: HashMap::new(),
                session_id: "session-1".to_string(),
            },
            &test_config(),
        )
        .expect("task should build");

        assert_eq!(task.working_dir, Some(PathBuf::from("src")));
    }
}
