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
) -> anyhow::Result<()> {
    tracing::info!("Initializing sandbox pool...");

    // Validate base image exists
    if !base_image.exists() {
        anyhow::bail!("Base image does not exist: {}", base_image.display());
    }

    let overlay_dir = overlay_dir.unwrap_or_else(PoolConfig::default_overlay_dir);

    // Create config
    let config = PoolConfig::builder()
        .pool_size(pool_size)
        .base_image(&base_image)
        .overlay_dir(&overlay_dir)
        .build()?;

    // Save config
    let config_file = config_path.unwrap_or_else(PoolConfig::default_config_path);
    if let Some(parent) = config_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    config.save_to_file(&config_file)?;

    tracing::info!("Configuration saved to: {}", config_file.display());

    // Create overlay directory
    std::fs::create_dir_all(&overlay_dir)?;
    tracing::info!("Overlay directory: {}", overlay_dir.display());

    // Test pool creation
    tracing::info!("Testing pool initialization...");
    let pool = Pool::new(config).await?;
    let status = pool.status();

    println!("Pool initialized successfully!");
    println!("  Total sandboxes: {}", status.total);
    println!("  Idle sandboxes: {}", status.idle);
    println!("  Config file: {}", config_file.display());
    println!("  Overlay dir: {}", overlay_dir.display());

    pool.shutdown().await;
    Ok(())
}

/// Run the daemon.
pub async fn run_daemon(bind: String, config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let config_file = config_path.unwrap_or_else(PoolConfig::default_config_path);

    if !config_file.exists() {
        anyhow::bail!(
            "Config file not found: {}. Run 'apiary init' first.",
            config_file.display()
        );
    }

    let config = PoolConfig::from_file(&config_file)?;
    tracing::info!("Loaded config from: {}", config_file.display());

    let pool = Arc::new(Pool::new(config).await?);
    tracing::info!("Pool initialized with {} sandboxes", pool.status().total);
    tracing::info!("Starting daemon API server (bind address: {bind})");

    let server_result = server::run_server(bind, pool.clone()).await;
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
) -> anyhow::Result<()> {
    let config_file = config_path.unwrap_or_else(PoolConfig::default_config_path);

    if !config_file.exists() {
        anyhow::bail!(
            "Config file not found: {}. Run 'apiary init' first.",
            config_file.display()
        );
    }

    let config = PoolConfig::from_file(&config_file)?;
    let pool = Pool::new(config.clone()).await?;

    // Parse environment variables
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

    // Create task
    let mut task = Task::new(&command)
        .timeout(Duration::from_secs(timeout))
        .envs(env_map);

    if let Some(dir) = workdir {
        task = task.working_dir(dir);
    } else {
        task = task.working_dir(&config.default_workdir);
    }

    tracing::info!("Executing: {command}");

    // Execute
    let result = pool.execute(task).await?;

    // Print output
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

    pool.shutdown().await;

    std::process::exit(result.exit_code);
}

/// Run multiple tasks from a JSON file.
pub async fn run_batch(
    tasks_file: PathBuf,
    parallelism: usize,
    config_path: Option<PathBuf>,
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

    let config = PoolConfig::from_file(&config_file)?;

    // Adjust pool size for parallelism
    let config = PoolConfig {
        pool_size: parallelism.min(config.pool_size),
        ..config
    };

    let pool = Pool::new(config).await?;

    // Load tasks
    let tasks_content = std::fs::read_to_string(&tasks_file)?;
    let tasks: Vec<Task> = serde_json::from_str(&tasks_content)?;

    tracing::info!("Loaded {} tasks from {}", tasks.len(), tasks_file.display());
    tracing::info!("Running with parallelism: {parallelism}");

    // Execute batch
    let start = std::time::Instant::now();
    let results = pool.execute_batch(tasks).await;
    let duration = start.elapsed();

    // Print summary
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
        std::process::exit(1);
    }

    Ok(())
}

/// Show pool status.
pub async fn show_status(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    let config_file = config_path.unwrap_or_else(PoolConfig::default_config_path);

    if !config_file.exists() {
        anyhow::bail!(
            "Config file not found: {}. Run 'apiary init' first.",
            config_file.display()
        );
    }

    let config = PoolConfig::from_file(&config_file)?;
    let pool = Pool::new(config).await?;
    let status = pool.status();

    println!("=== Sandbox Pool Status ===");
    println!("Total sandboxes: {}", status.total);
    println!("Idle: {}", status.idle);
    println!("Busy: {}", status.busy);
    println!("Error: {}", status.error);
    println!();
    println!("=== Statistics ===");
    println!("Tasks executed: {}", status.tasks_executed);
    println!("Succeeded: {}", status.tasks_succeeded);
    println!("Failed: {}", status.tasks_failed);
    if status.tasks_executed > 0 {
        println!("Success rate: {:.1}%", 
            status.tasks_succeeded as f64 / status.tasks_executed as f64 * 100.0);
        println!("Avg duration: {}ms", status.avg_task_duration_ms);
    }

    pool.shutdown().await;
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

    // Remove overlay directory
    if config.overlay_dir.exists() {
        tracing::info!("Removing overlay directory: {}", config.overlay_dir.display());
        std::fs::remove_dir_all(&config.overlay_dir)?;
    }

    // Remove config file
    if config_file.exists() {
        tracing::info!("Removing config file: {}", config_file.display());
        std::fs::remove_file(&config_file)?;
    }

    println!("Cleanup complete.");
    Ok(())
}

/// Create a sample tasks JSON file.
#[allow(dead_code)]
pub fn create_sample_tasks_file(path: &PathBuf) -> anyhow::Result<()> {
    let tasks = vec![
        Task::new("echo 'Hello from task 1'"),
        Task::new("echo 'Hello from task 2'"),
        Task::new("ls -la /"),
        Task::builder()
            .command("python3 -c 'print(2 + 2)'")
            .timeout_secs(30)
            .build()
            .unwrap(),
    ];

    let json = serde_json::to_string_pretty(&tasks)?;
    std::fs::write(path, json)?;

    println!("Sample tasks file created: {}", path.display());
    Ok(())
}
