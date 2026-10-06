//! `mage`: the one binary of ahomage (README "Crates"). Loads the config file, routes its role
//! sections, installs telemetry, and runs `mage start server | indexer | runner | all` or an
//! operator command.

mod assets;
mod cli;
mod ops;
mod roles;
mod start;

use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use mage_infra::telemetry::{LogFormat, TelemetryConfig};
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command};

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("mage: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    let path = cli.config.context("no config file: pass --config <path> or set MAGE_CONFIG")?;
    let file = roles::load(&path, roles::env_vars())?;
    match cli.command {
        Command::Start { role } => {
            let roles = roles::route(&file, role)?;
            mage_infra::telemetry::init(&roles::telemetry(&file, &roles)?)?;
            start::run(roles).await
        }
        command => {
            operator_logging()?;
            ops::run(&file, command).await
        }
    }
}

/// Operator commands keep stdout for their results: warnings and errors go to stderr as text
/// (`RUST_LOG` overrides the level).
fn operator_logging() -> Result<()> {
    let config = TelemetryConfig { format: LogFormat::Text, filter: "warn".to_owned(), ..TelemetryConfig::new("mage-cli") };
    let filter = match std::env::var("RUST_LOG") {
        Ok(directives) if !directives.is_empty() => EnvFilter::try_new(directives),
        _ => EnvFilter::try_new(&config.filter),
    }?;
    tracing::subscriber::set_global_default(mage_infra::telemetry::subscriber(&config, filter, std::io::stderr))?;
    Ok(())
}
