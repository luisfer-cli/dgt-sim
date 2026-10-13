//! `dgt-sim` entry point: configure logging, parse the CLI, run it.

use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use dgt_sim::cli::{Cli, run};

fn main() -> ExitCode {
    // clap handles `--help`/`--version` and usage errors before any of the
    // simulator exists, so no board is ever created for a bad invocation.
    let cli = Cli::parse();
    init_tracing(cli.verbose, cli.quiet);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("dgt-sim: cannot start the async runtime: {err}");
            return ExitCode::from(1);
        }
    };

    match runtime.block_on(run(cli)) {
        Ok(code) => ExitCode::from(code as u8),
        Err(err) => {
            eprintln!("dgt-sim: {err}");
            ExitCode::from(1)
        }
    }
}

/// Logs go to stderr so `--json` on stdout stays machine-readable.
fn init_tracing(verbose: u8, quiet: bool) {
    let default = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let directive = if quiet { "off" } else { default };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(directive));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}
