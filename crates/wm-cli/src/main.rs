//! `wm` — the WireMirage CLI.
//!
//! Thin shim over the REST API per [[cli-design.md]]. The CLI itself
//! does no logic beyond formatting input into HTTP requests via
//! `wm_core::Client` and rendering responses for human or machine
//! consumption.

mod cli;
mod config;
mod format;
mod handlers;
mod spec;

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

    // Resolve profile / env / flag layering into the dispatch-ready
    // (host, token) pair before any subcommand runs. Failures here
    // (bad config file, named profile that doesn't exist) are
    // reported in the same shape as any other startup error.
    let config = match config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e:#}");
            return ExitCode::from(1);
        }
    };
    let effective = match config.resolve(args.profile.as_deref(), args.host, args.token) {
        Ok(eff) => eff,
        Err(e) => {
            eprintln!("error: {e:#}");
            return ExitCode::from(1);
        }
    };

    match handlers::dispatch(effective.host, effective.token, args.json, args.command).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}
