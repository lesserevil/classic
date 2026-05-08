use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Parser, Debug)]
#[command(name = "classicd", version, about = "Classic SSI node daemon")]
struct Args {
    /// Override the config-file search path. If supplied but missing, the
    /// daemon exits 1; we never fall back when an explicit path is given.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    init_tracing();

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("classicd: failed to start tokio runtime: {e}");
            return ExitCode::from(70); // EX_SOFTWARE
        }
    };

    match runtime.block_on(classic_node::run(args.config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "classicd exiting");
            ExitCode::from(e.exit_code() as u8)
        }
    }
}

fn init_tracing() {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer().with_target(true))
        .try_init();
}
