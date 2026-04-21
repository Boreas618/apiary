//! # Sandbox Pool
//!
//! A lightweight sandbox pool for running AI agent tasks on Linux with namespace isolation.
//!
//! ## Features
//!
//! - **Namespace Isolation**: User, Mount, and PID namespace isolation for each sandbox
//! - **OverlayFS**: Shared read-only base with per-sandbox writable layers
//! - **seccomp**: Network syscall filtering for security
//! - **cgroups v2**: Resource limits (CPU, memory, PIDs, I/O)
//! - **Rootless**: Can run without root privileges (Linux 5.11+)
//! - **On-demand Sandboxes**: Dedicated sandbox per session, created on-demand
//!
//! ## Example
//!
//! ```rust,no_run
//! use apiary::{LayerCacheConfig, Pool, PoolConfig, SessionOptions, Task};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let config = PoolConfig::builder()
//!         .max_sandboxes(16)
//!         .image_cache(LayerCacheConfig {
//!             layers_dir: "/tmp/apiary_layers".into(),
//!             docker: "docker".into(),
//!             pull_concurrency: 8,
//!         })
//!         .build()?;
//!
//!     let pool = Pool::new(config).await?;
//!
//!     // Register an image at runtime via the image loader.
//!     pool.image_loader().load_one("ubuntu:22.04", |_| {}).await;
//!
//!     let task = Task::new("echo hello")
//!         .timeout(std::time::Duration::from_secs(30));
//!
//!     let session_id = pool
//!         .create_session(SessionOptions::new("ubuntu:22.04", "/workspace"))
//!         .await?;
//!     let result = pool.execute_in_session(&session_id, task).await?;
//!     println!("Exit code: {}", result.exit_code);
//!     pool.close_session(&session_id).await?;
//!
//!     Ok(())
//! }
//! ```

pub mod config;
pub mod images;
pub mod pool;
pub mod sandbox;
pub mod task;

pub use config::{
    LayerCacheConfig, OverlayDriver, PoolConfig, PoolConfigBuilder, ResourceLimits, SeccompPolicy,
};
pub use images::{ImageLoader, ImageRegistry, ImageStage, LoadOutcome};
pub use pool::{
    ImageJob, ImageJobState, ImageJobs, ImageProgress, JobAck, JobId, Pool, PoolError, PoolStatus,
    SessionOptions,
};
pub use sandbox::{Sandbox, SandboxError, SandboxState};
pub use task::{MountSpec, Task, TaskBuilder, TaskResult};
