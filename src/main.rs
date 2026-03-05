use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

mod api;


#[derive(Parser)]
#[command(name = "apiary")]
#[command(author, version, about = "A lightweight sandbox pool for running tasks with isolation")]
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

        /// Number of sandboxes in the pool
        #[arg(long, default_value = "10")]
        pool_size: usize,

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

        /// Working directory inside the sandbox
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
        #[arg(long, default_value = "10")]
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

fn setup_logging(verbose: u8) {
    let filter = match verbose {
        0 => "apiary=info",
        1 => "apiary=debug",
        _ => "apiary=trace",
    };

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter)))
        .init();
}

fn main() -> anyhow::Result<()> {
    apiary::sandbox::namespace::enter_rootless_mode()?;

    tokio::runtime::Runtime::new()?.block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    setup_logging(cli.verbose);

    let seccomp = cli.seccomp;

    match cli.command {
        Commands::Init {
            base_image,
            pool_size,
            overlay_dir,
        } => {
            api::cli::init_pool(base_image, pool_size, overlay_dir, cli.config, seccomp).await?;
        }
        Commands::Daemon { bind, api_token } => {
            api::cli::run_daemon(bind, api_token, cli.config, seccomp).await?;
        }
        Commands::Run {
            command,
            timeout,
            workdir,
            env,
        } => {
            api::cli::run_task(command, timeout, workdir, env, cli.config, seccomp).await?;
        }
        Commands::Batch { tasks, parallelism } => {
            api::cli::run_batch(tasks, parallelism, cli.config, seccomp).await?;
        }
        Commands::Status => {
            api::cli::show_status(cli.config).await?;
        }
        Commands::Clean { force } => {
            api::cli::cleanup(force, cli.config).await?;
        }
    }

    Ok(())
}
