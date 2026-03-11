use base64::Engine as _;
use rmcp::{
    handler::server::wrapper::Json as Parameters,
    model::*,
    schemars, tool,
    service::RequestContext,
    tool_router, tool_handler,
    handler::server::router::tool::ToolRouter,
    ServerHandler, RoleServer,
};

use crate::apiary_client::ExecuteTaskResponse;
use crate::session::SessionManager;

/// Extract client_id from the MCP session ID header.
fn extract_client_id(ctx: &RequestContext<RoleServer>) -> String {
    ctx.extensions
        .get::<http::request::Parts>()
        .and_then(|parts| parts.headers.get("mcp-session-id"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("stdio")
        .to_owned()
}

fn format_result(resp: &ExecuteTaskResponse) -> String {
    let mut parts = Vec::new();
    if !resp.stdout.is_empty() {
        parts.push(resp.stdout.clone());
    }
    if !resp.stderr.is_empty() {
        parts.push(format!("[stderr]\n{}", resp.stderr));
    }
    if resp.timed_out {
        parts.push("[timed out]".to_owned());
    }
    if resp.exit_code != 0 {
        parts.push(format!("[exit code: {}]", resp.exit_code));
    }
    if parts.is_empty() {
        "(no output)".to_owned()
    } else {
        parts.join("\n")
    }
}

fn q(s: &str) -> String {
    shell_escape::escape(s.into()).into_owned()
}

// --- Tool parameter structs ---

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ShellExecParams {
    /// Shell command (interpreted by bash; pipes, redirects, etc. work).
    pub command: String,
    /// Maximum execution time in milliseconds (default 30000).
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Override the working directory for this command.
    #[serde(default)]
    pub working_dir: Option<String>,
}
fn default_timeout_ms() -> u64 {
    30_000
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReadFileParams {
    /// Absolute or workspace-relative path.
    pub path: String,
    /// Maximum bytes to read (default 1000000).
    #[serde(default = "default_byte_limit")]
    pub byte_limit: u64,
}
fn default_byte_limit() -> u64 {
    1_000_000
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct WriteFileParams {
    /// Absolute or workspace-relative file path.
    pub path: String,
    /// Text content to write.
    pub content: String,
    /// If true, append to the file instead of overwriting.
    #[serde(default)]
    pub append: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ListDirectoryParams {
    /// Directory path (default: working directory).
    #[serde(default = "default_dot")]
    pub path: String,
    /// Include hidden (dot) files.
    #[serde(default)]
    pub show_hidden: bool,
}
fn default_dot() -> String {
    ".".to_owned()
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CreateDirectoryParams {
    /// Directory path to create.
    pub path: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RemoveParams {
    /// Path to remove.
    pub path: String,
    /// Remove directories and all their contents.
    #[serde(default)]
    pub recursive: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct MoveFileParams {
    /// Current path.
    pub source: String,
    /// New path.
    pub destination: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CopyFileParams {
    /// Source path.
    pub source: String,
    /// Destination path.
    pub destination: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FileInfoParams {
    /// Path to inspect.
    pub path: String,
}

// --- Handler ---

#[derive(Clone)]
pub struct ApiaryMcpHandler {
    session_mgr: SessionManager,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl ApiaryMcpHandler {
    pub fn new(session_mgr: SessionManager) -> Self {
        Self {
            session_mgr,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Execute a shell command inside an isolated Linux sandbox. \
        The sandbox keeps filesystem state between calls — installed packages, \
        created files, and environment changes all persist as long as the same \
        client_id is used.")]
    async fn shell_exec(
        &self,
        Parameters(params): Parameters<ShellExecParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cid = extract_client_id(&ctx);
        let resp = self
            .session_mgr
            .execute(
                &cid,
                &params.command,
                Some(params.timeout_ms),
                params.working_dir.as_deref(),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(format_result(
            &resp,
        ))]))
    }

    #[tool(description = "Read a file from the sandbox.")]
    async fn read_file(
        &self,
        Parameters(params): Parameters<ReadFileParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cid = extract_client_id(&ctx);
        let cmd = format!("head -c {} -- {}", params.byte_limit, q(&params.path));
        let resp = self
            .session_mgr
            .execute(&cid, &cmd, None, None)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        if resp.exit_code != 0 {
            let msg = format!("Error reading file: {}", resp.stderr.trim());
            return Ok(CallToolResult::success(vec![Content::text(msg)]));
        }
        Ok(CallToolResult::success(vec![Content::text(resp.stdout)]))
    }

    #[tool(description = "Write text to a file in the sandbox. Creates parent directories as needed. \
        Content is base64-transported to safely handle arbitrary characters.")]
    async fn write_file(
        &self,
        Parameters(params): Parameters<WriteFileParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cid = extract_client_id(&ctx);
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(params.content.as_bytes());
        let redir = if params.append { ">>" } else { ">" };
        let cmd = format!(
            "mkdir -p -- \"$(dirname {})\" && printf '%s' {} | base64 -d {} {}",
            q(&params.path),
            q(&encoded),
            redir,
            q(&params.path)
        );
        let resp = self
            .session_mgr
            .execute(&cid, &cmd, None, None)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        if resp.exit_code != 0 {
            let msg = format!("Error writing file: {}", resp.stderr.trim());
            return Ok(CallToolResult::success(vec![Content::text(msg)]));
        }
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Wrote {} bytes to {}",
            params.content.len(),
            params.path
        ))]))
    }

    #[tool(description = "List directory contents in the sandbox.")]
    async fn list_directory(
        &self,
        Parameters(params): Parameters<ListDirectoryParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cid = extract_client_id(&ctx);
        let flags = if params.show_hidden { "-lhA" } else { "-lh" };
        let cmd = format!("ls {} -- {}", flags, q(&params.path));
        let resp = self
            .session_mgr
            .execute(&cid, &cmd, None, None)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        if resp.exit_code != 0 {
            let msg = format!("Error listing directory: {}", resp.stderr.trim());
            return Ok(CallToolResult::success(vec![Content::text(msg)]));
        }
        Ok(CallToolResult::success(vec![Content::text(resp.stdout)]))
    }

    #[tool(description = "Create a directory (and any missing parents) in the sandbox.")]
    async fn create_directory(
        &self,
        Parameters(params): Parameters<CreateDirectoryParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cid = extract_client_id(&ctx);
        let cmd = format!("mkdir -p -- {}", q(&params.path));
        let resp = self
            .session_mgr
            .execute(&cid, &cmd, None, None)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        if resp.exit_code != 0 {
            let msg = format!("Error creating directory: {}", resp.stderr.trim());
            return Ok(CallToolResult::success(vec![Content::text(msg)]));
        }
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Created directory: {}",
            params.path
        ))]))
    }

    #[tool(description = "Remove a file or directory from the sandbox.")]
    async fn remove(
        &self,
        Parameters(params): Parameters<RemoveParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cid = extract_client_id(&ctx);
        let flags = if params.recursive { "-rf" } else { "-f" };
        let cmd = format!("rm {} -- {}", flags, q(&params.path));
        let resp = self
            .session_mgr
            .execute(&cid, &cmd, None, None)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        if resp.exit_code != 0 {
            let msg = format!("Error removing: {}", resp.stderr.trim());
            return Ok(CallToolResult::success(vec![Content::text(msg)]));
        }
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Removed: {}",
            params.path
        ))]))
    }

    #[tool(description = "Move or rename a file/directory in the sandbox.")]
    async fn move_file(
        &self,
        Parameters(params): Parameters<MoveFileParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cid = extract_client_id(&ctx);
        let cmd = format!("mv -- {} {}", q(&params.source), q(&params.destination));
        let resp = self
            .session_mgr
            .execute(&cid, &cmd, None, None)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        if resp.exit_code != 0 {
            let msg = format!("Error moving: {}", resp.stderr.trim());
            return Ok(CallToolResult::success(vec![Content::text(msg)]));
        }
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Moved {} -> {}",
            params.source, params.destination
        ))]))
    }

    #[tool(description = "Copy a file or directory in the sandbox.")]
    async fn copy_file(
        &self,
        Parameters(params): Parameters<CopyFileParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cid = extract_client_id(&ctx);
        let cmd = format!("cp -r -- {} {}", q(&params.source), q(&params.destination));
        let resp = self
            .session_mgr
            .execute(&cid, &cmd, None, None)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        if resp.exit_code != 0 {
            let msg = format!("Error copying: {}", resp.stderr.trim());
            return Ok(CallToolResult::success(vec![Content::text(msg)]));
        }
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Copied {} -> {}",
            params.source, params.destination
        ))]))
    }

    #[tool(description = "Get metadata (type, size, permissions, timestamps) for a path in the sandbox.")]
    async fn file_info(
        &self,
        Parameters(params): Parameters<FileInfoParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let cid = extract_client_id(&ctx);
        let cmd = format!(
            "stat -- {} 2>&1; file -- {} 2>&1",
            q(&params.path),
            q(&params.path)
        );
        let resp = self
            .session_mgr
            .execute(&cid, &cmd, None, None)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        if resp.exit_code != 0 {
            let msg = format!("Error getting info: {}", resp.stderr.trim());
            return Ok(CallToolResult::success(vec![Content::text(msg)]));
        }
        Ok(CallToolResult::success(vec![Content::text(resp.stdout)]))
    }
}

#[tool_handler]
impl ServerHandler for ApiaryMcpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "apiary-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Apiary sandbox MCP server. Provides shell execution and file operations \
                 inside isolated Linux sandboxes."
                    .to_string(),
            )
    }
}
