//! CLI command implementations.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use apiary::{Pool, PoolConfig, PoolError, SessionOptions, Task};
use clap::{Parser, Subcommand};
use tokio::task::JoinSet;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::api::server;

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

    /// Enable seccomp syscall filtering inside sandboxes
    #[arg(long, global = true)]
    seccomp: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize the sandbox pool
    Init {
        /// Path to base rootfs image
        #[arg(long)]
        base_image: PathBuf,

        /// Minimum number of sandboxes (created at startup)
        #[arg(long, default_value = "10")]
        min_sandboxes: usize,

        /// Maximum number of sandboxes (hard ceiling for auto-scaling)
        #[arg(long, default_value = "40")]
        max_sandboxes: usize,

        /// Number of sandboxes to create per scale-up event
        #[arg(long, default_value = "2")]
        scale_up_step: usize,

        /// How long (seconds) an excess sandbox can be idle before removal
        #[arg(long, default_value = "300")]
        idle_timeout_secs: u64,

        /// Minimum seconds between scaling events
        #[arg(long, default_value = "30")]
        cooldown_secs: u64,

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

    /// Run a single command in a sandbox
    Run {
        /// Command to execute
        #[arg(long)]
        command: String,

        /// Timeout in seconds
        #[arg(long, default_value = "60")]
        timeout: u64,

        /// Default session working directory inside the sandbox
        #[arg(long)]
        workdir: Option<PathBuf>,

        /// Environment variables (KEY=VALUE)
        #[arg(long, short = 'e')]
        env: Vec<String>,
    },

    /// Run multiple tasks from a JSON file
    Batch {
        /// Path to tasks JSON file
        #[arg(long)]
        tasks: PathBuf,

        /// Maximum parallel tasks
        #[arg(long, default_value = "10", value_parser = clap::value_parser!(usize).range(1..))]
        parallelism: usize,
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
    let seccomp = cli.seccomp;

    match cli.command {
        Commands::Init {
            base_image,
            min_sandboxes,
            max_sandboxes,
            scale_up_step,
            idle_timeout_secs,
            cooldown_secs,
            overlay_dir,
        } => {
            init_pool(
                base_image,
                min_sandboxes,
                max_sandboxes,
                scale_up_step,
                Duration::from_secs(idle_timeout_secs),
                Duration::from_secs(cooldown_secs),
                overlay_dir,
                config_path,
                seccomp,
            )
            .await?;
        }
        Commands::Daemon { bind, api_token } => {
            run_daemon(bind, api_token, config_path, seccomp).await?;
        }
        Commands::Run {
            command,
            timeout,
            workdir,
            env,
        } => {
            run_task(command, timeout, workdir, env, config_path, seccomp).await?;
        }
        Commands::Batch { tasks, parallelism } => {
            run_batch(tasks, parallelism, config_path, seccomp).await?;
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

/// Resolve config path, load from file, and optionally enable seccomp.
fn load_config(
    config_path: Option<PathBuf>,
    enable_seccomp: bool,
) -> anyhow::Result<(PoolConfig, PathBuf)> {
    let config_file = resolve_config_path(config_path);
    if !config_file.exists() {
        anyhow::bail!(
            "Config file not found: {}. Run 'apiary init' first.",
            config_file.display()
        );
    }

    let mut config = PoolConfig::from_file(&config_file)?;
    if enable_seccomp {
        config = config.with_seccomp_enabled(true);
    }

    tracing::info!("Loaded config from: {}", config_file.display());
    Ok((config, config_file))
}

/// Initialize the sandbox pool.
pub async fn init_pool(
    base_image: PathBuf,
    min_sandboxes: usize,
    max_sandboxes: usize,
    scale_up_step: usize,
    idle_timeout: Duration,
    cooldown: Duration,
    overlay_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    enable_seccomp: bool,
) -> anyhow::Result<()> {
    tracing::info!("Initializing sandbox pool...");

    if !base_image.exists() {
        anyhow::bail!("Base image does not exist: {}", base_image.display());
    }

    let overlay_dir = overlay_dir.unwrap_or_else(PoolConfig::default_overlay_dir);
    let config = PoolConfig::builder()
        .min_sandboxes(min_sandboxes)
        .max_sandboxes(max_sandboxes)
        .scale_up_step(scale_up_step)
        .idle_timeout(idle_timeout)
        .cooldown(cooldown)
        .base_image(&base_image)
        .overlay_dir(&overlay_dir)
        .enable_seccomp(enable_seccomp)
        .build()?;

    let config_file = resolve_config_path(config_path);
    if let Some(parent) = config_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    config.save_to_file(&config_file)?;

    tracing::info!("Configuration saved to: {}", config_file.display());

    std::fs::create_dir_all(&overlay_dir)?;
    tracing::info!("Overlay directory: {}", overlay_dir.display());

    tracing::info!("Testing pool initialization...");
    let pool = Pool::new(config).await?;
    let status = pool.status();

    println!("Pool initialized successfully!");
    println!(
        "  Sandboxes: {} (min={}, max={})",
        status.total, status.min_sandboxes, status.max_sandboxes
    );
    println!("  Scale-up step: {scale_up_step}");
    println!("  Idle timeout: {idle_timeout:?}");
    println!("  Cooldown: {cooldown:?}");
    println!(
        "  Seccomp: {}",
        if enable_seccomp {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("  Config file: {}", config_file.display());
    println!("  Overlay dir: {}", overlay_dir.display());

    pool.shutdown().await;
    Ok(())
}

/// Run the daemon.
pub async fn run_daemon(
    bind: String,
    api_token: Option<String>,
    config_path: Option<PathBuf>,
    enable_seccomp: bool,
) -> anyhow::Result<()> {
    let (config, _) = load_config(config_path, enable_seccomp)?;

    let pool = Pool::new(config).await?;
    tracing::info!("Pool initialized with {} sandboxes", pool.status().total);
    tracing::info!("Starting daemon API server (bind address: {bind})");

    let server_result = server::run_server(bind, pool.clone(), api_token).await;
    tracing::info!("Shutting down...");
    pool.shutdown().await;
    server_result?;

    Ok(())
}

/// Run a single task.
pub async fn run_task(
    command: String,
    timeout: u64,
    workdir: Option<PathBuf>,
    env: Vec<String>,
    config_path: Option<PathBuf>,
    enable_seccomp: bool,
) -> anyhow::Result<()> {
    let (config, _) = load_config(config_path, enable_seccomp)?;
    let pool = Pool::new(config).await?;

    let env_map: HashMap<String, String> = env
        .iter()
        .filter_map(|entry| {
            let (key, value) = entry.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect();

    let task = Task::new(&command)
        .timeout(Duration::from_secs(timeout))
        .envs(env_map);

    let session_options = workdir
        .map(|dir| SessionOptions::default().working_dir(dir))
        .unwrap_or_default();

    tracing::info!("Executing: {command}");

    let result = match pool.run_task(task, session_options).await {
        Ok(result) => result,
        Err(error) => {
            pool.shutdown().await;
            return Err(error.into());
        }
    };

    if !result.stdout.is_empty() {
        print!("{}", result.stdout_lossy());
    }
    if !result.stderr.is_empty() {
        eprint!("{}", result.stderr_lossy());
    }

    println!();
    println!("Exit code: {}", result.exit_code);
    println!("Duration: {:?}", result.duration);

    if result.timed_out {
        println!("Status: TIMEOUT");
    } else if result.exit_code == 0 {
        println!("Status: SUCCESS");
    } else {
        println!("Status: FAILED");
    }

    let exit_code = result.exit_code;
    pool.shutdown().await;

    if exit_code != 0 {
        anyhow::bail!("task exited with code {exit_code}");
    }

    Ok(())
}

/// Run multiple tasks from a JSON file.
pub async fn run_batch(
    tasks_file: PathBuf,
    parallelism: usize,
    config_path: Option<PathBuf>,
    enable_seccomp: bool,
) -> anyhow::Result<()> {
    if parallelism == 0 {
        anyhow::bail!("parallelism must be at least 1");
    }
    if !tasks_file.exists() {
        anyhow::bail!("Tasks file not found: {}", tasks_file.display());
    }

    let (config, _) = load_config(config_path, enable_seccomp)?;
    let capped_min = parallelism.min(config.min_sandboxes);
    let capped_max = parallelism.min(config.max_sandboxes);
    let config = config.with_pool_bounds(capped_min, capped_max)?;

    let tasks_content = std::fs::read_to_string(&tasks_file)?;
    let tasks: Vec<Task> = serde_json::from_str(&tasks_content)?;

    let pool = Pool::new(config).await?;

    tracing::info!("Loaded {} tasks from {}", tasks.len(), tasks_file.display());
    tracing::info!("Running with parallelism: {parallelism} (session-only mode)");

    let start = std::time::Instant::now();
    let results = run_batch_tasks(pool.clone(), tasks, parallelism).await?;
    let duration = start.elapsed();

    let mut succeeded = 0;
    let mut failed = 0;
    let mut timed_out = 0;

    for (idx, result) in results.iter().enumerate() {
        match result {
            Ok(task_result) => {
                if task_result.timed_out {
                    timed_out += 1;
                    println!("Task {}: TIMEOUT", idx + 1);
                } else if task_result.exit_code == 0 {
                    succeeded += 1;
                    println!("Task {}: SUCCESS", idx + 1);
                } else {
                    failed += 1;
                    println!("Task {}: FAILED (exit {})", idx + 1, task_result.exit_code);
                }
            }
            Err(error) => {
                failed += 1;
                println!("Task {}: ERROR ({})", idx + 1, error);
            }
        }
    }

    println!();
    println!("=== Batch Summary ===");
    println!("Total tasks: {}", results.len());
    println!("Succeeded: {succeeded}");
    println!("Failed: {failed}");
    println!("Timed out: {timed_out}");
    println!("Total duration: {duration:?}");
    println!(
        "Average per task: {:?}",
        duration / results.len().max(1) as u32
    );

    pool.shutdown().await;

    if failed > 0 || timed_out > 0 {
        anyhow::bail!("{failed} task(s) failed, {timed_out} timed out");
    }

    Ok(())
}

async fn run_batch_tasks(
    pool: Pool,
    tasks: Vec<Task>,
    parallelism: usize,
) -> anyhow::Result<Vec<Result<apiary::TaskResult, PoolError>>> {
    let mut pending = tasks.into_iter().enumerate();
    let mut in_flight = JoinSet::new();
    let mut results = Vec::new();

    for _ in 0..parallelism {
        let Some((index, task)) = pending.next() else {
            break;
        };
        let pool = pool.clone();
        in_flight
            .spawn(async move { (index, pool.run_task(task, SessionOptions::default()).await) });
    }

    while let Some(joined) = in_flight.join_next().await {
        let (index, result) =
            joined.map_err(|error| anyhow::anyhow!("batch task join failed: {error}"))?;
        results.push((index, result));

        if let Some((next_index, next_task)) = pending.next() {
            let pool = pool.clone();
            in_flight.spawn(async move {
                (
                    next_index,
                    pool.run_task(next_task, SessionOptions::default()).await,
                )
            });
        }
    }

    results.sort_by_key(|(index, _)| *index);
    Ok(results.into_iter().map(|(_, result)| result).collect())
}

/// Show pool configuration (reads config without creating a pool).
pub async fn show_status(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let (config, config_file) = load_config(config_path, false)?;

    println!("=== Sandbox Pool Configuration ===");
    println!("Config file: {}", config_file.display());
    println!("Min sandboxes: {}", config.min_sandboxes);
    println!("Max sandboxes: {}", config.max_sandboxes);
    println!("Scale-up step: {}", config.scale_up_step);
    println!("Idle timeout: {:?}", config.idle_timeout);
    println!("Cooldown: {:?}", config.cooldown);
    println!("Base image: {}", config.base_image.display());
    println!("Overlay dir: {}", config.overlay_dir.display());
    println!("Overlay driver: {:?}", config.overlay_driver);
    println!("Default timeout: {:?}", config.default_timeout);
    println!("Default workdir: {}", config.default_workdir.display());
    println!(
        "Seccomp: {}",
        if config.enable_seccomp {
            "enabled"
        } else {
            "disabled (use --seccomp to enable)"
        }
    );
    println!();
    println!("=== Resource Limits ===");
    println!("Memory max: {}", config.resource_limits.memory_max);
    println!("CPU max: {}", config.resource_limits.cpu_max);
    println!("PIDs max: {}", config.resource_limits.pids_max);
    if let Some(ref io_max) = config.resource_limits.io_max {
        println!("I/O max: {io_max}");
    }

    if config.enable_seccomp {
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
