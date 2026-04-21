//! HTTP server for daemon mode.
//!
//! Routes (all under `auth_layer` if `--api-token` is set):
//!
//! - `GET    /healthz`
//! - `GET    /api/v1/status`
//! - `POST   /api/v1/sessions` — create a session for a registered image
//! - `DELETE /api/v1/sessions/{id}`
//! - `POST   /api/v1/sessions/{id}/exec`
//! - `GET    /api/v1/images` — list registered images
//! - `POST   /api/v1/images` — submit an async image-load job
//! - `DELETE /api/v1/images/{name}` — drop an image from the registry
//! - `GET    /api/v1/images/jobs/{job_id}` — poll image-load job status

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

use apiary::{
    ImageJob, JobAck, Pool, PoolConfig, PoolError, PoolStatus, SessionOptions, Task,
};

#[derive(Clone)]
struct AppState {
    pool: Pool,
    api_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecuteTaskRequest {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    working_dir: Option<PathBuf>,
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateSessionRequest {
    working_dir: PathBuf,
    image: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterImagesRequest {
    images: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ListImagesResponse {
    images: Vec<String>,
    count: usize,
}

#[derive(Debug, Serialize)]
struct ExecuteTaskResponse {
    task_id: String,
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
    // An empty token is never a valid Bearer credential: with `Some("")` the
    // auth layer would require `Authorization: Bearer ` (trailing space, empty
    // token), which no reasonable HTTP client emits, locking the API out.
    // `clap`'s `env = "APIARY_API_TOKEN"` also yields `Some("")` whenever the
    // env var is exported as empty (a common outcome of
    // `APIARY_API_TOKEN: "${APIARY_API_TOKEN:-}"` in Compose).
    let api_token = api_token.filter(|t| !t.is_empty());

    let state = AppState {
        pool,
        api_token: api_token.clone(),
    };

    let api_routes = Router::new()
        .route("/api/v1/status", get(status))
        .route("/api/v1/sessions", post(create_session))
        .route("/api/v1/sessions/{session_id}", delete(close_session))
        .route("/api/v1/sessions/{session_id}/exec", post(execute_task))
        .route("/api/v1/images", get(list_images).post(register_images))
        .route("/api/v1/images/{name}", delete(unregister_image))
        .route("/api/v1/images/jobs/{job_id}", get(image_job_status))
        .layer(middleware::from_fn_with_state(state.clone(), auth_layer));

    let app = Router::new()
        .route("/healthz", get(healthz))
        .merge(api_routes)
        .with_state(state);

    if api_token.is_some() {
        tracing::info!("API authentication enabled (Bearer token required)");
    } else {
        tracing::info!("API authentication disabled (no APIARY_API_TOKEN configured)");
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
    Json(payload): Json<CreateSessionRequest>,
) -> Result<Json<CreateSessionResponse>, ApiError> {
    let session_options = SessionOptions::new(payload.image, payload.working_dir);
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
    Path(session_id): Path<String>,
    Json(payload): Json<ExecuteTaskRequest>,
) -> Result<Response, ApiError> {
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
        exit_code: result.exit_code,
        timed_out: result.timed_out,
        duration_ms: result.duration.as_millis(),
        success,
        stdout,
        stderr,
    })
    .into_response())
}

// ---------------------------------------------------------------------------
// Image registry endpoints
// ---------------------------------------------------------------------------

async fn list_images(State(state): State<AppState>) -> Json<ListImagesResponse> {
    let images = state.pool.image_registry().list();
    let count = images.len();
    Json(ListImagesResponse { images, count })
}

async fn register_images(
    State(state): State<AppState>,
    Json(payload): Json<RegisterImagesRequest>,
) -> Result<(StatusCode, Json<JobAck>), ApiError> {
    let images: Vec<String> = payload
        .images
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if images.is_empty() {
        return Err(ApiError::bad_request(
            "request body `images` must contain at least one non-empty name",
        ));
    }

    let ack = state.pool.image_jobs().submit(images);
    Ok((StatusCode::ACCEPTED, Json(ack)))
}

async fn unregister_image(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let removed = state.pool.image_registry().remove(&name);
    if removed.is_some() {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found(format!("image not registered: {name}")))
    }
}

async fn image_job_status(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<ImageJob>, ApiError> {
    state
        .pool
        .image_jobs()
        .status(&job_id)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("image job not found: {job_id}")))
}

fn build_task(payload: ExecuteTaskRequest, config: &PoolConfig) -> Result<Task, ApiError> {
    let command = payload.command.trim();
    if command.is_empty() {
        return Err(ApiError::bad_request("command must not be empty"));
    }

    let timeout = payload
        .timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(config.default_timeout);

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

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
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
            PoolError::AtCapacity(_) => Self {
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
            PoolError::UnknownImage(name) => Self {
                status: StatusCode::NOT_FOUND,
                message: format!(
                    "image not registered: {name}. POST /api/v1/images to register it first."
                ),
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
    use serde_json::json;

    fn test_config() -> PoolConfig {
        PoolConfig::builder()
            .image_cache(apiary::LayerCacheConfig {
                layers_dir: std::path::PathBuf::from("/tmp/test_layers"),
                docker: "docker".to_string(),
                pull_concurrency: 1,
            })
            .default_timeout(Duration::from_secs(42))
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
                working_dir: None,
                env: HashMap::new(),
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
                working_dir: Some(PathBuf::from("src")),
                env: HashMap::new(),
            },
            &test_config(),
        )
        .expect("task should build");

        assert_eq!(task.working_dir, Some(PathBuf::from("src")));
    }

    #[test]
    fn build_task_rejects_blank_commands() {
        let error = build_task(
            ExecuteTaskRequest {
                command: "   ".to_string(),
                timeout_ms: None,
                working_dir: None,
                env: HashMap::new(),
            },
            &test_config(),
        )
        .expect_err("blank commands should be rejected");

        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(error.message, "command must not be empty");
    }

    #[test]
    fn build_task_uses_timeout_ms_and_overrides_default_env() {
        let mut env = HashMap::new();
        env.insert("DEFAULT_KEY".to_string(), "override".to_string());
        env.insert("EXTRA_KEY".to_string(), "extra-value".to_string());

        let task = build_task(
            ExecuteTaskRequest {
                command: "echo hello".to_string(),
                timeout_ms: Some(1_500),
                working_dir: None,
                env,
            },
            &test_config(),
        )
        .expect("task should build");

        assert_eq!(task.timeout, Duration::from_millis(1_500));
        assert_eq!(task.env.get("DEFAULT_KEY"), Some(&"override".to_string()));
        assert_eq!(task.env.get("EXTRA_KEY"), Some(&"extra-value".to_string()));
    }

    #[test]
    fn execute_task_request_rejects_removed_timeout_secs_field() {
        let error = serde_json::from_value::<ExecuteTaskRequest>(json!({
            "command": "echo hello",
            "timeout_secs": 30
        }))
        .expect_err("unknown timeout field should be rejected");

        assert!(error.to_string().contains("unknown field `timeout_secs`"));
    }

    #[test]
    fn unknown_image_pool_error_maps_to_404() {
        let err = ApiError::from_pool_error(PoolError::UnknownImage("ubuntu:22.04".to_string()));
        assert_eq!(err.status, StatusCode::NOT_FOUND);
        assert!(err.message.contains("ubuntu:22.04"));
        assert!(err.message.contains("POST /api/v1/images"));
    }

    #[test]
    fn register_images_request_rejects_unknown_fields() {
        let error = serde_json::from_value::<RegisterImagesRequest>(json!({
            "images": ["ubuntu:22.04"],
            "unexpected": true
        }))
        .expect_err("unknown fields should be rejected");
        assert!(error.to_string().contains("unknown field"));
    }
}
