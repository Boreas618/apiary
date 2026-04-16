//! CLI command implementations.

use std::path::PathBuf;

use apiary::{ImagesConfig, Pool, PoolConfig};
use clap::{Parser, Subcommand};
use tokio::task::JoinSet;
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
    /// Initialize the sandbox pool
    Init {
        /// Docker image names or paths to image-list files (repeatable).
        #[arg(long = "image", required = true)]
        images: Vec<String>,

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

        /// Bearer token for API authentication (also reads APIARY_API_TOKEN env)
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
            images,
            layers_dir,
            max_sandboxes,
            overlay_dir,
        } => {
            init_pool(
                images,
                layers_dir,
                max_sandboxes,
                overlay_dir,
                config_path,
            )
            .await?;
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

/// Initialize the sandbox pool.
pub async fn init_pool(
    images: Vec<String>,
    layers_dir: PathBuf,
    max_sandboxes: usize,
    overlay_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    tracing::info!("Initializing sandbox pool...");

    let overlay_dir = overlay_dir.unwrap_or_else(PoolConfig::default_overlay_dir);
    let images_config = ImagesConfig {
        sources: images,
        layers_dir,
        docker: "docker".to_string(),
        pull_concurrency: 8,
    };

    let config = PoolConfig::builder()
        .max_sandboxes(max_sandboxes)
        .images(images_config)
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

    pull_missing_images(&config).await?;

    tracing::info!("Testing pool initialization...");
    let pool = Pool::new(config).await?;

    println!("Pool initialized successfully!");
    println!("  Max sandboxes: {max_sandboxes}");
    println!("  Seccomp: enabled");
    println!("  Config file: {}", config_file.display());
    println!("  Overlay dir: {}", overlay_dir.display());

    pool.shutdown().await;
    Ok(())
}

/// Pull any images that are not yet available in the local Docker daemon.
async fn pull_missing_images(config: &PoolConfig) -> anyhow::Result<()> {
    let names = config.images.all_image_names()?;
    let docker = &config.images.docker;

    let missing: Vec<_> = names
        .iter()
        .filter(|name| {
            let output = std::process::Command::new(docker)
                .args(["inspect", "--format", "{{.Id}}", name.as_str()])
                .output();
            !matches!(output, Ok(o) if o.status.success())
        })
        .cloned()
        .collect();

    if missing.is_empty() {
        tracing::info!("All {} images available locally", names.len());
        return Ok(());
    }

    let concurrency = config.images.pull_concurrency;
    println!(
        "Pulling {} missing image(s) (concurrency={concurrency})...",
        missing.len()
    );

    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency.max(1)));
    let mut set = JoinSet::new();
    for img in missing {
        let bin = docker.clone();
        let permit_owner = sem.clone();
        set.spawn(async move {
            let _permit = permit_owner.acquire_owned().await;
            let output = tokio::process::Command::new(&bin)
                .args(["pull", &img])
                .output()
                .await;
            (img, output)
        });
    }

    let mut failed = 0usize;
    while let Some(res) = set.join_next().await {
        let (img, output) = res?;
        match output {
            Ok(o) if o.status.success() => println!("  pulled {img}"),
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                eprintln!("  FAILED {img}: {}", stderr.trim());
                failed += 1;
            }
            Err(e) => {
                eprintln!("  FAILED {img}: {e}");
                failed += 1;
            }
        }
    }

    if failed > 0 {
        anyhow::bail!("{failed} image pull(s) failed");
    }
    println!("All pulls succeeded.");
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
    tracing::info!("Pool initialized with {} sandboxes", pool.status().total);
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
    println!("Config file: {}", config_file.display());
    println!("Max sandboxes: {}", config.max_sandboxes);
    println!("Layers dir: {}", config.images.layers_dir.display());
    println!("Image sources: {}", config.images.sources.len());
    println!("Overlay dir: {}", config.overlay_dir.display());
    println!("Overlay driver: {:?}", config.overlay_driver);
    println!(
        "Mount host resolv.conf into sandbox: {}",
        config.mount_host_resolv_conf
    );
    println!("Default timeout: {:?}", config.default_timeout);
    println!("Seccomp: enabled");
    println!();
    println!("=== Resource Limits ===");
    println!("Memory max: {}", config.resource_limits.memory_max);
    println!("CPU max: {}", config.resource_limits.cpu_max);
    println!("PIDs max: {}", config.resource_limits.pids_max);
    if let Some(ref io_max) = config.resource_limits.io_max {
        println!("I/O max: {io_max}");
    }

    println!();
    println!("=== Seccomp Policy ===");
    println!("Block network: {}", config.seccomp_policy.block_network);
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
