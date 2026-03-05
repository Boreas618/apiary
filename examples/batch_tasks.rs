//! Example: Running batch tasks in the sandbox pool.
//!
//! This example demonstrates how to use the sandbox pool to execute
//! multiple tasks in parallel with isolation.
//!
//! Note: This requires a valid base rootfs image. You can create one with:
//! ```bash
//! # Using debootstrap (Ubuntu/Debian)
//! sudo debootstrap --variant=minbase focal ./rootfs
//!
//! # Or using Docker
//! docker export $(docker create ubuntu:20.04) | tar -xf - -C ./rootfs
//! ```

use apiary::{Pool, PoolConfig, Task};
use std::sync::Arc;
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    apiary::sandbox::namespace::enter_rootless_mode()?;
    tokio::runtime::Runtime::new()?.block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // Check for rootfs argument
    let args: Vec<String> = std::env::args().collect();
    let rootfs = args.get(1).map(|s| s.as_str()).unwrap_or("./rootfs");

    if !std::path::Path::new(rootfs).exists() {
        eprintln!("Error: Base rootfs not found at: {rootfs}");
        eprintln!();
        eprintln!("Please create a rootfs or specify the path:");
        eprintln!("  {} /path/to/rootfs", args[0]);
        eprintln!();
        eprintln!("To create a minimal rootfs:");
        eprintln!("  # Using Docker:");
        eprintln!("  docker export $(docker create alpine:latest) | tar -xf - -C ./rootfs");
        std::process::exit(1);
    }

    // Create pool configuration
    println!("Creating sandbox pool with base image: {rootfs}");
    let config = PoolConfig::builder()
        .pool_size(4)
        .base_image(rootfs)
        .build()?;

    // Initialize the pool
    println!("Initializing pool...");
    let pool = Arc::new(Pool::new(config).await?);
    println!("Pool ready: {} sandboxes", pool.status().total);

    // Create a batch of tasks
    let tasks = vec![
        Task::new("echo 'Task 1: Hello World'"),
        Task::new("echo 'Task 2: Computing...' && sleep 1 && echo 'Done!'"),
        Task::new("ls -la /"),
        Task::builder()
            .command("sh -c 'for i in 1 2 3; do echo \"Count: $i\"; sleep 0.5; done'")
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build task"),
        Task::new("cat /etc/os-release || echo 'No os-release found'"),
    ];

    println!();
    println!("Submitting {} tasks...", tasks.len());
    println!();

    // Execute all tasks in parallel (each task gets its own session).
    let start = std::time::Instant::now();
    let results = futures::future::join_all(tasks.into_iter().map(|task| {
        let pool = pool.clone();
        async move {
            let session_id = pool.create_session().await?;
            let execution_result = pool.execute_in_session(&session_id, task).await;
            let close_result = pool.close_session(&session_id).await;

            match close_result {
                Ok(()) => execution_result,
                Err(close_error) => {
                    if execution_result.is_ok() {
                        Err(close_error)
                    } else {
                        tracing::error!(
                            %close_error,
                            session_id = %session_id,
                            "failed to close session after task error"
                        );
                        execution_result
                    }
                }
            }
        }
    }))
    .await;
    let elapsed = start.elapsed();

    // Print results
    println!("=== Results ===");
    println!();

    for (i, result) in results.iter().enumerate() {
        println!("--- Task {} ---", i + 1);
        match result {
            Ok(r) => {
                println!("Exit code: {}", r.exit_code);
                println!("Duration: {:?}", r.duration);
                if r.timed_out {
                    println!("Status: TIMEOUT");
                }
                if !r.stdout.is_empty() {
                    println!("Stdout:");
                    for line in r.stdout_lossy().lines() {
                        println!("  {line}");
                    }
                }
                if !r.stderr.is_empty() {
                    println!("Stderr:");
                    for line in r.stderr_lossy().lines() {
                        println!("  {line}");
                    }
                }
            }
            Err(e) => {
                println!("Error: {e}");
            }
        }
        println!();
    }

    // Print summary
    let succeeded = results.iter().filter(|r| r.as_ref().is_ok_and(|r| r.success())).count();
    let failed = results.len() - succeeded;

    println!("=== Summary ===");
    println!("Total tasks: {}", results.len());
    println!("Succeeded: {succeeded}");
    println!("Failed: {failed}");
    println!("Total time: {elapsed:?}");
    println!();

    // Show pool status
    let status = pool.status();
    println!("=== Pool Status ===");
    println!("Total sandboxes: {}", status.total);
    println!("Idle: {}", status.idle);
    println!("Tasks executed: {}", status.tasks_executed);

    // Cleanup
    pool.shutdown().await;
    println!();
    println!("Pool shutdown complete.");

    Ok(())
}
