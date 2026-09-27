mod benchmark;
mod cli;
mod optimizer;
mod state;
mod system;
mod ui;

fn main() -> anyhow::Result<()> {
    let arguments = cli::Arguments::parse();

    match arguments.command {
        Some(command) => cli::run(command),
        None => ui::run().map_err(|error| anyhow::anyhow!(error.to_string())),
    }
}
