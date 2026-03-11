use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Clone)]
pub struct ApiaryClient {
    client: Client,
    base_url: String,
}

#[derive(Debug, Serialize)]
pub struct CreateSessionRequest {
    pub working_dir: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateSessionResponse {
    pub session_id: String,
}

#[derive(Debug, Serialize)]
pub struct ExecuteTaskRequest {
    pub command: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct ExecuteTaskResponse {
    pub task_id: String,
    pub session_id: String,
    pub exit_code: i32,
    pub timed_out: bool,
    pub duration_ms: u64,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiaryError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Session not found: {0}")]
    SessionNotFound(String),
    #[error("Apiary error ({status}): {body}")]
    ApiError { status: u16, body: String },
}

impl ApiaryClient {
    pub fn new(base_url: &str, token: Option<&str>) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        if let Some(tok) = token {
            headers.insert(
                "authorization",
                format!("Bearer {tok}").parse().unwrap(),
            );
        }

        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(300))
            .pool_max_idle_per_host(200)
            .build()
            .expect("failed to build reqwest client");

        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
        }
    }

    pub async fn create_session(&self, working_dir: &str) -> Result<String, ApiaryError> {
        let resp = self
            .client
            .post(format!("{}/api/v1/sessions", self.base_url))
            .json(&CreateSessionRequest {
                working_dir: working_dir.to_owned(),
            })
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiaryError::ApiError { status, body });
        }

        let data: CreateSessionResponse = resp.json().await?;
        Ok(data.session_id)
    }

    pub async fn destroy_session(&self, session_id: &str) -> Result<(), ApiaryError> {
        let resp = self
            .client
            .delete(format!(
                "{}/api/v1/sessions/{session_id}",
                self.base_url
            ))
            .send()
            .await?;
        if resp.status().as_u16() == 404 {
            return Ok(()); // already gone
        }
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiaryError::ApiError { status, body });
        }
        Ok(())
    }

    pub async fn execute(
        &self,
        req: &ExecuteTaskRequest,
    ) -> Result<ExecuteTaskResponse, ApiaryError> {
        let resp = self
            .client
            .post(format!("{}/api/v1/tasks", self.base_url))
            .json(req)
            .send()
            .await?;

        if resp.status().as_u16() == 404 {
            return Err(ApiaryError::SessionNotFound(req.session_id.clone()));
        }
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiaryError::ApiError { status, body });
        }
        Ok(resp.json().await?)
    }
}
