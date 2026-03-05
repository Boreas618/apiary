//! CLI command implementations.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use apiary::{Pool, PoolConfig, Task};

use crate::api::server;

/// Initialize the sandbox pool.
pub async fn init_pool(
    base_image: PathBuf,
    pool_size: usize,
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
        .pool_size(pool_size)
        .base_image(&base_image)
        .overlay_dir(&overlay_dir)
        .enable_seccomp(enable_seccomp)
        .build()?;

    let config_file = config_path.unwrap_or_else(PoolConfig::default_config_path);
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
    println!("  Total sandboxes: {}", status.total);
    println!("  Idle sandboxes: {}", status.idle);
    println!("  Seccomp: {}", if enable_seccomp { "enabled" } else { "disabled" });
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
    let config_file = config_path.unwrap_or_else(PoolConfig::default_config_path);

    if !config_file.exists() {
        anyhow::bail!(
            "Config file not found: {}. Run 'apiary init' first.",
            config_file.display()
        );
    }

    let mut config = PoolConfig::from_file(&config_file)?;
    if enable_seccomp {
        config.enable_seccomp = true;
    }
    tracing::info!("Loaded config from: {}", config_file.display());

    let pool = Arc::new(Pool::new(config).await?);
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
    let config_file = config_path.unwrap_or_else(PoolConfig::default_config_path);

    if !config_file.exists() {
        anyhow::bail!(
            "Config file not found: {}. Run 'apiary init' first.",
            config_file.display()
        );
    }

    let mut config = PoolConfig::from_file(&config_file)?;
    if enable_seccomp {
        config.enable_seccomp = true;
    }
    let pool = Pool::new(config.clone()).await?;

    let env_map: HashMap<String, String> = env
        .iter()
        .filter_map(|s| {
            let parts: Vec<&str> = s.splitn(2, '=').collect();
            if parts.len() == 2 {
                Some((parts[0].to_string(), parts[1].to_string()))
            } else {
                None
            }
        })
        .collect();

    let mut task = Task::new(&command)
        .timeout(Duration::from_secs(timeout))
        .envs(env_map);

    if let Some(dir) = workdir {
        task = task.working_dir(dir);
    } else {
        task = task.working_dir(&config.default_workdir);
    }

    tracing::info!("Executing: {command}");

    let result = pool.execute(task).await?;

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
    let config_file = config_path.unwrap_or_else(PoolConfig::default_config_path);

    if !config_file.exists() {
        anyhow::bail!(
            "Config file not found: {}. Run 'apiary init' first.",
            config_file.display()
        );
    }

    if !tasks_file.exists() {
        anyhow::bail!("Tasks file not found: {}", tasks_file.display());
    }

    let mut config = PoolConfig::from_file(&config_file)?;
    if enable_seccomp {
        config.enable_seccomp = true;
    }

    let config = PoolConfig {
        pool_size: parallelism.min(config.pool_size),
        ..config
    };

    let pool = Pool::new(config).await?;

    let tasks_content = std::fs::read_to_string(&tasks_file)?;
    let tasks: Vec<Task> = serde_json::from_str(&tasks_content)?;

    tracing::info!("Loaded {} tasks from {}", tasks.len(), tasks_file.display());
    tracing::info!("Running with parallelism: {parallelism}");

    let start = std::time::Instant::now();
    let results = pool.execute_batch(tasks).await;
    let duration = start.elapsed();

    let mut succeeded = 0;
    let mut failed = 0;
    let mut timed_out = 0;

    for (i, result) in results.iter().enumerate() {
        match result {
            Ok(r) => {
                if r.timed_out {
                    timed_out += 1;
                    println!("Task {}: TIMEOUT", i + 1);
                } else if r.exit_code == 0 {
                    succeeded += 1;
                    println!("Task {}: SUCCESS", i + 1);
                } else {
                    failed += 1;
                    println!("Task {}: FAILED (exit {})", i + 1, r.exit_code);
                }
            }
            Err(e) => {
                failed += 1;
                println!("Task {}: ERROR ({})", i + 1, e);
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

/// Show pool configuration (reads config without creating a pool).
pub async fn show_status(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let config_file = config_path.unwrap_or_else(PoolConfig::default_config_path);

    if !config_file.exists() {
        anyhow::bail!(
            "Config file not found: {}. Run 'apiary init' first.",
            config_file.display()
        );
    }

    let config = PoolConfig::from_file(&config_file)?;

    println!("=== Sandbox Pool Configuration ===");
    println!("Config file: {}", config_file.display());
    println!("Pool size: {}", config.pool_size);
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
    let config_file = config_path.unwrap_or_else(PoolConfig::default_config_path);

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
        tracing::info!("Removing overlay directory: {}", config.overlay_dir.display());
        std::fs::remove_dir_all(&config.overlay_dir)?;
    }

    if config_file.exists() {
        tracing::info!("Removing config file: {}", config_file.display());
        std::fs::remove_file(&config_file)?;
    }

    println!("Cleanup complete.");
    Ok(())
}
