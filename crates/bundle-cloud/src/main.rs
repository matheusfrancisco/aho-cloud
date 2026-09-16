//! `ahoc`: the aho cloud product serving the aho rpc protocol (NDJSON) on stdio for one session.
//!
//! `ahoc [--project DIR] [--set K=V]… [--disable ID]… rpc [--session HEX | --memory]`
//!
//! The flags match what `aho_sdk::Spawn` passes, so any aho rpc client can launch it.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use aho_sdk::{Open, Sdk};
use aho_types::SessionId;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "ahoc", about = "The aho cloud product (aho-lab + cloud_draft, cloud_propose, edge contract) over aho rpc on stdio.")]
struct Cli {
    /// Set a config key for this invocation (the Arg layer). Repeatable.
    #[arg(long = "set", value_name = "KEY=VALUE", global = true, value_parser = key_value)]
    sets: Vec<(String, String)>,
    /// Disable a capability for this invocation. Repeatable.
    #[arg(long = "disable", value_name = "ID", global = true)]
    disable: Vec<String>,
    /// Project directory (default: the current directory).
    #[arg(long, value_name = "DIR", global = true)]
    project: Option<PathBuf>,
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Subcommand, Debug)]
enum Verb {
    /// Serve the NDJSON rpc protocol on stdio for one session.
    Rpc {
        /// Resume this session (its hex id, as `aho session list` shows).
        #[arg(long, value_name = "HEX")]
        session: Option<String>,
        /// Keep the log in memory only.
        #[arg(long, conflicts_with = "session")]
        memory: bool,
    },
}

fn key_value(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .filter(|(k, _)| !k.trim().is_empty())
        .map(|(k, v)| (k.trim().to_string(), v.to_string()))
        .ok_or_else(|| format!("`{s}` is not KEY=VALUE"))
}

fn open(session: Option<&str>, memory: bool) -> Result<Open, String> {
    Ok(match session {
        Some(hex) => Open::Resume(SessionId(
            u128::from_str_radix(hex, 16)
                .map_err(|e| format!("session id `{hex}` is not hex ({e}); pass the id `aho session list` shows"))?,
        )),
        None if memory => Open::InMemory,
        None => Open::New,
    })
}

async fn rpc(cli: &Cli, session: Option<&str>, memory: bool) -> Result<(), String> {
    let open = open(session, memory)?;
    let mut b = Sdk::builder().inherit_env().composition(bundle_cloud::cloud());
    if let Some(p) = &cli.project {
        b = b.project(p);
    }
    for (k, v) in &cli.sets {
        b = b.set(k, v);
    }
    for id in &cli.disable {
        b = b.disable(id);
    }
    let sdk = b.build().map_err(|e| e.to_string())?;
    sdk.serve_io(open, tokio::io::stdin(), tokio::io::stdout()).await.map(|_| ()).map_err(|e| e.to_string())
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.verb {
        Verb::Rpc { session, memory } => rpc(&cli, session.as_deref(), *memory).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ahoc: {e}");
            ExitCode::FAILURE
        }
    }
}
