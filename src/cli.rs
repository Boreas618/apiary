//! CLI command implementations.
//!
//! Three commands:
//!
//! - `init` writes a fresh config file and prepares overlay/cache dirs.
//!   It does **not** know anything about images — they are registered at
//!   runtime via the daemon's HTTP API.
//! - `daemon` loads the config and starts the HTTP server.
//! - `status` / `clean` are utility commands that read the config.

use std::path::PathBuf;

use apiary::{LayerCacheConfig, Pool, PoolConfig};
use clap::{Parser, Subcommand};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::server;

#[derive(Parser)]
#[command(name = "apiary")]
#[command(
    author,
    version,
    about = "A lightweight sandbox pool for running tasks with isolation"
)]
struct Cli {
    /// Config file path
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// Verbose output
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize the sandbox pool (config + directories).
    ///
    /// No images are pre-loaded. Clients register images at runtime via
    /// `POST /api/v1/images` against a running daemon.
    Init {
        /// Local directory for content-addressable layer cache
        #[arg(long, default_value = "/tmp/apiary_layers")]
        layers_dir: PathBuf,

        /// Maximum number of sandboxes (hard ceiling for concurrent sessions)
        #[arg(long, default_value = "40")]
        max_sandboxes: usize,

        /// Directory to store overlay layers
        #[arg(long)]
        overlay_dir: Option<PathBuf>,
    },

    /// Start the sandbox pool daemon
    Daemon {
        /// Bind address for the API server
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: String,

        /// Bearer token for API authentication (also reads APIARY_API_TOKEN env).
        /// Unset or empty disables authentication entirely.
        #[arg(long, env = "APIARY_API_TOKEN")]
        api_token: Option<String>,
    },

    /// Show pool configuration and status
    Status,

    /// Clean up sandbox pool resources
    Clean {
        /// Force cleanup even if sandboxes are running
        #[arg(long)]
        force: bool,
    },
}

pub fn main() -> anyhow::Result<()> {
    tokio::runtime::Runtime::new()?.block_on(async_main())
}

fn setup_logging(verbose: u8) {
    let default_filter = match verbose {
        0 => "apiary=info",
        1 => "apiary=debug",
        _ => "apiary=trace",
    };

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::builder().parse_lossy(default_filter));

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(env_filter)
        .init();
}

async fn async_main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    setup_logging(cli.verbose);

    let config_path = cli.config;

    match cli.command {
        Commands::Init {
            layers_dir,
            max_sandboxes,
            overlay_dir,
        } => {
            init_pool(layers_dir, max_sandboxes, overlay_dir, config_path).await?;
        }
        Commands::Daemon { bind, api_token } => {
            run_daemon(bind, api_token, config_path).await?;
        }
        Commands::Status => {
            show_status(config_path).await?;
        }
        Commands::Clean { force } => {
            cleanup(force, config_path).await?;
        }
    }

    Ok(())
}

fn resolve_config_path(config_path: Option<PathBuf>) -> PathBuf {
    config_path.unwrap_or_else(PoolConfig::default_config_path)
}

/// Resolve config path and load from file.
fn load_config(config_path: Option<PathBuf>) -> anyhow::Result<(PoolConfig, PathBuf)> {
    let config_file = resolve_config_path(config_path);
    if !config_file.exists() {
        anyhow::bail!(
            "Config file not found: {}. Run 'apiary init' first.",
            config_file.display()
        );
    }

    let config = PoolConfig::from_file(&config_file)?;
    tracing::info!("Loaded config from: {}", config_file.display());
    Ok((config, config_file))
}

/// Initialize the sandbox pool: write config, create directories.
///
/// No images are pulled or extracted here — the registry is populated
/// at runtime via the HTTP API.
pub async fn init_pool(
    layers_dir: PathBuf,
    max_sandboxes: usize,
    overlay_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    tracing::info!("Initializing sandbox pool config...");

    let overlay_dir = overlay_dir.unwrap_or_else(PoolConfig::default_overlay_dir);
    let image_cache = LayerCacheConfig {
        layers_dir,
        docker: "docker".to_string(),
        pull_concurrency: 8,
    };

    let config = PoolConfig::builder()
        .max_sandboxes(max_sandboxes)
        .image_cache(image_cache)
        .overlay_dir(&overlay_dir)
        .build()?;

    let config_file = resolve_config_path(config_path);
    if let Some(parent) = config_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    config.save_to_file(&config_file)?;

    tracing::info!("Configuration saved to: {}", config_file.display());

    std::fs::create_dir_all(&overlay_dir)?;
    tracing::info!("Overlay directory: {}", overlay_dir.display());

    std::fs::create_dir_all(&config.image_cache.layers_dir)?;
    tracing::info!(
        "Layer cache directory: {}",
        config.image_cache.layers_dir.display()
    );

    println!("Pool initialised successfully!");
    println!("  Max sandboxes:  {max_sandboxes}");
    println!("  Seccomp:        enabled");
    println!("  Config file:    {}", config_file.display());
    println!("  Overlay dir:    {}", overlay_dir.display());
    println!(
        "  Layer cache:    {}",
        config.image_cache.layers_dir.display()
    );
    println!();
    println!("Registry starts empty. Register images at runtime:");
    println!("  curl -X POST http://<host>:<port>/api/v1/images \\");
    println!("    -H 'Content-Type: application/json' \\");
    println!("    -d '{{\"images\": [\"ubuntu:22.04\"]}}'");

    Ok(())
}

/// Run the daemon.
pub async fn run_daemon(
    bind: String,
    api_token: Option<String>,
    config_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    let (config, _) = load_config(config_path)?;

    let pool = Pool::new(config).await?;
    tracing::info!(
        "Pool initialised; registry starts empty (POST /api/v1/images to register)",
    );
    tracing::info!("Starting daemon API server (bind address: {bind})");

    let server_result = server::run_server(bind, pool.clone(), api_token).await;
    tracing::info!("Shutting down...");
    pool.shutdown().await;
    server_result?;

    Ok(())
}

/// Show pool configuration (reads config without creating a pool).
pub async fn show_status(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let (config, config_file) = load_config(config_path)?;

    println!("=== Sandbox Pool Configuration ===");
    println!("Config file:    {}", config_file.display());
    println!("Max sandboxes:  {}", config.max_sandboxes);
    println!("Layer cache:    {}", config.image_cache.layers_dir.display());
    println!("Docker bin:     {}", config.image_cache.docker);
    println!("Pull concurrency: {}", config.image_cache.pull_concurrency);
    println!("Overlay dir:    {}", config.overlay_dir.display());
    println!("Overlay driver: {:?}", config.overlay_driver);
    println!(
        "Mount host resolv.conf into sandbox: {}",
        config.mount_host_resolv_conf
    );
    println!("Default timeout: {:?}", config.default_timeout);
    println!("Seccomp:        enabled");
    println!();
    println!("=== Resource Limits ===");
    println!("Memory max:     {}", config.resource_limits.memory_max);
    println!("CPU max:        {}", config.resource_limits.cpu_max);
    println!("PIDs max:       {}", config.resource_limits.pids_max);
    if let Some(ref io_max) = config.resource_limits.io_max {
        println!("I/O max:        {io_max}");
    }

    println!();
    println!("=== Seccomp Policy ===");
    println!("Block network:  {}", config.seccomp_policy.block_network);
    println!(
        "Allow UNIX sockets: {}",
        config.seccomp_policy.allow_unix_sockets
    );
    if !config.seccomp_policy.blocked_syscalls.is_empty() {
        println!(
            "Additional blocked: {}",
            config.seccomp_policy.blocked_syscalls.join(", ")
        );
    }
    if !config.seccomp_policy.allowed_syscalls.is_empty() {
        println!(
            "Explicitly allowed: {}",
            config.seccomp_policy.allowed_syscalls.join(", ")
        );
    }

    println!();
    println!("For live pool status, query the daemon API: GET /api/v1/status");
    println!("For registered images,                    GET /api/v1/images");

    Ok(())
}

/// Clean up sandbox pool resources.
pub async fn cleanup(force: bool, config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let config_file = resolve_config_path(config_path);

    if !config_file.exists() {
        println!("No pool configuration found. Nothing to clean.");
        return Ok(());
    }

    let config = PoolConfig::from_file(&config_file)?;

    if !force {
        println!("This will remove all sandbox data:");
        println!("  Overlay dir: {}", config.overlay_dir.display());
        println!("  Config file: {}", config_file.display());
        println!();
        println!("Are you sure? Use --force to confirm.");
        return Ok(());
    }

    if config.overlay_dir.exists() {
        tracing::info!(
            "Removing overlay directory: {}",
            config.overlay_dir.display()
        );
        std::fs::remove_dir_all(&config.overlay_dir)?;
    }

    if config_file.exists() {
        tracing::info!("Removing config file: {}", config_file.display());
        std::fs::remove_file(&config_file)?;
    }

    println!("Cleanup complete.");
    Ok(())
}
