mod cli;
mod server;

fn main() -> anyhow::Result<()> {
    apiary::sandbox::namespace::enter_rootless_mode()?;
    cli::main()
}
