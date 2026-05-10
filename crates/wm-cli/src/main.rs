//! `wm` — the WireMirage CLI.
//!
//! Thin shim over the REST API per [[cli-design.md]]. The CLI itself
//! does no logic beyond formatting input into HTTP requests via
//! `wm_core::Client` and rendering responses for human or machine
//! consumption.

mod cli;
mod format;
mod handlers;

use std::process::ExitCode;

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to start tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    runtime.block_on(async { run().await })
}

async fn run() -> ExitCode {
    use clap::Parser;
    let args = cli::Cli::parse();
    match handlers::dispatch(args).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
