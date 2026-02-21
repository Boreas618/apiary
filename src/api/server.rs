//! HTTP server for daemon mode.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use apiary::{Pool, PoolError, Task, TaskOutputEvent};

#[derive(Clone)]
struct AppState {
    pool: Arc<Pool>,
}

#[derive(Debug, Deserialize)]
struct RunTaskQuery {
    #[serde(default)]
    stream: bool,
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
struct StatusResponse {
    total: usize,
    idle: usize,
    busy: usize,
    error: usize,
    tasks_executed: u64,
    tasks_succeeded: u64,
    tasks_failed: u64,
    avg_task_duration_ms: u64,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Debug, Serialize)]
struct StreamStartEvent {
    task_id: String,
}

#[derive(Debug, Serialize)]
struct StreamChunkEvent {
    stream: &'static str,
    data: String,
}

#[derive(Debug, Serialize)]
struct StreamDoneEvent {
    task_id: String,
    exit_code: i32,
    timed_out: bool,
    duration_ms: u128,
    success: bool,
}

pub async fn run_server(bind: String, pool: Arc<Pool>) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/status", get(status))
        .route("/api/v1/tasks", post(execute_task))
        .with_state(AppState { pool });

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    let local_addr = listener.local_addr()?;
    tracing::info!("API server listening on http://{local_addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
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
        error: status.error,
        tasks_executed: status.tasks_executed,
        tasks_succeeded: status.tasks_succeeded,
        tasks_failed: status.tasks_failed,
        avg_task_duration_ms: status.avg_task_duration_ms,
    })
}

async fn execute_task(
    State(state): State<AppState>,
    Query(query): Query<RunTaskQuery>,
    Json(payload): Json<ExecuteTaskRequest>,
) -> Result<Response, ApiError> {
    let task = build_task(payload, &state.pool)?;

    if query.stream {
        return Ok(stream_task(state.pool.clone(), task).into_response());
    }

    let result = state
        .pool
        .execute(task)
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

fn stream_task(
    pool: Arc<Pool>,
    task: Task,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let task_id = task.id.clone();
    let (output_tx, mut output_rx) = mpsc::unbounded_channel::<TaskOutputEvent>();
    let (done_tx, mut done_rx) = oneshot::channel::<Result<apiary::TaskResult, PoolError>>();

    tokio::spawn(async move {
        let result = pool.execute_with_events(task, output_tx).await;
        let _ = done_tx.send(result);
    });

    let event_stream = stream! {
        yield Ok(json_event(
            "task_started",
            &StreamStartEvent {
                task_id: task_id.clone(),
            },
        ));

        let mut output_open = true;
        loop {
            tokio::select! {
                maybe_event = output_rx.recv(), if output_open => {
                    match maybe_event {
                        Some(event) => {
                            let payload = match event {
                                TaskOutputEvent::Stdout(bytes) => StreamChunkEvent {
                                    stream: "stdout",
                                    data: String::from_utf8_lossy(&bytes).into_owned(),
                                },
                                TaskOutputEvent::Stderr(bytes) => StreamChunkEvent {
                                    stream: "stderr",
                                    data: String::from_utf8_lossy(&bytes).into_owned(),
                                },
                            };
                            yield Ok(json_event("task_output", &payload));
                        }
                        None => {
                            output_open = false;
                        }
                    }
                }
                result = &mut done_rx => {
                    match result {
                        Ok(Ok(task_result)) => {
                            let success = task_result.success();
                            yield Ok(json_event(
                                "task_done",
                                &StreamDoneEvent {
                                    task_id: task_result.task_id,
                                    exit_code: task_result.exit_code,
                                    timed_out: task_result.timed_out,
                                    duration_ms: task_result.duration.as_millis(),
                                    success,
                                },
                            ));
                        }
                        Ok(Err(pool_error)) => {
                            yield Ok(json_event(
                                "error",
                                &ErrorResponse {
                                    error: pool_error.to_string(),
                                },
                            ));
                        }
                        Err(recv_error) => {
                            yield Ok(json_event(
                                "error",
                                &ErrorResponse {
                                    error: format!("task result channel failed: {recv_error}"),
                                },
                            ));
                        }
                    }
                    break;
                }
            }
        }
    };

    Sse::new(event_stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(10))
            .text("keep-alive"),
    )
}

fn json_event<T: Serialize>(name: &str, payload: &T) -> Event {
    match serde_json::to_string(payload) {
        Ok(json) => Event::default().event(name).data(json),
        Err(e) => Event::default()
            .event("error")
            .data(format!("failed to serialize {name} payload: {e}")),
    }
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

    let mut task = Task::new(command).timeout(timeout).envs(env);
    if let Some(working_dir) = payload.working_dir {
        task = task.working_dir(working_dir);
    } else {
        task = task.working_dir(pool.config().default_workdir.clone());
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
            PoolError::NoIdleSandbox => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: "no idle sandbox available".to_string(),
            },
            PoolError::ShuttingDown => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: "pool is shutting down".to_string(),
            },
            PoolError::TaskSubmissionFailed(message) => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: format!("task submission failed: {message}"),
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
