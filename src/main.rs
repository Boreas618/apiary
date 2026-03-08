mod api;

fn main() -> anyhow::Result<()> {
    apiary::sandbox::namespace::enter_rootless_mode()?;
    api::cli::main()
}
