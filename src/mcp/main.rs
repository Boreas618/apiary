mod apiary_client;
mod session;
mod tools;

use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use clap::Parser;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpService;
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::ServiceExt;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use apiary_client::ApiaryClient;
use session::SessionManager;
use tools::ApiaryMcpHandler;

#[derive(Parser)]
#[command(name = "apiary-mcp")]
#[command(about = "MCP server for shell & file operations in Apiary sandboxes")]
struct Cli {
    /// Bind host
    #[arg(long, default_value = "0.0.0.0")]
    host: String,

    /// Bind port
    #[arg(long, default_value = "8082")]
    port: u16,

    /// Apiary daemon URL
    #[arg(long, env = "APIARY_URL", default_value = "http://127.0.0.1:8080")]
    apiary_url: String,

    /// Apiary daemon bearer token
    #[arg(long, env = "APIARY_API_TOKEN")]
    apiary_token: Option<String>,

    /// Require this bearer token on the MCP endpoint
    #[arg(long, env = "MCP_AUTH_TOKEN")]
    mcp_token: Option<String>,

    /// Default sandbox working directory
    #[arg(long, env = "APIARY_WORKING_DIR", default_value = "/workspace")]
    working_dir: String,

    /// Seconds before an unconnected sandbox is reaped
    #[arg(long, default_value = "300")]
    idle_timeout: u64,

    /// MCP transport
    #[arg(long, default_value = "streamable-http", value_parser = ["streamable-http", "stdio"])]
    transport: String,
}

#[derive(Clone)]
struct AppState {
    session_mgr: SessionManager,
    mcp_token: Option<String>,
}

/// Constant-time byte comparison to prevent timing attacks on token validation.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

async fn auth_middleware(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if let Some(ref expected) = state.mcp_token {
        let provided = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "));
        match provided {
            Some(tok) if constant_time_eq(tok.as_bytes(), expected.as_bytes()) => {}
            _ => return Err(StatusCode::UNAUTHORIZED),
        }
    }
    Ok(next.run(request).await)
}

async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "sessions": state.session_mgr.active_sessions(),
    }))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    let apiary_client = ApiaryClient::new(&cli.apiary_url, cli.apiary_token.as_deref());
    let session_mgr = SessionManager::new(
        apiary_client,
        cli.working_dir.clone(),
        Duration::from_secs(cli.idle_timeout),
    );
    session_mgr.start_reaper();

    let handler = ApiaryMcpHandler::new(session_mgr.clone());

    if cli.transport == "stdio" {
        tracing::info!("Starting MCP server on stdio");
        let service = handler.serve(rmcp::transport::io::stdio()).await?;
        service.waiting().await?;
        session_mgr.shutdown().await;
        return Ok(());
    }

    // --- Streamable HTTP mode ---
    let app_state = AppState {
        session_mgr: session_mgr.clone(),
        mcp_token: cli.mcp_token.clone(),
    };

    let mcp_config = StreamableHttpServerConfig {
        stateful_mode: true,
        ..Default::default()
    };

    let mcp_service: StreamableHttpService<ApiaryMcpHandler> = StreamableHttpService::new(
        move || Ok(handler.clone()),
        LocalSessionManager::default().into(),
        mcp_config,
    );

    let app = Router::new()
        .route("/health", get(health))
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(
            app_state.clone(),
            auth_middleware,
        ))
        .with_state(app_state);

    let addr: SocketAddr = format!("{}:{}", cli.host, cli.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("MCP server listening on http://{addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("Shutting down...");
        })
        .await?;

    session_mgr.shutdown().await;
    Ok(())
}
