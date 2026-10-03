//! `ahocloud`: the customer CLI. Drafts a deploy manifest from repo files and aho session logs,
//! checks it against the gate locally, uploads source and session, proposes releases, and signs
//! approvals with the device key bound at `ahocloud login`. Manages sandboxes and opens terminals
//! in them (`ahocloud ssh`).
//!
//! Exit status: 0 success, 1 the gate refused the manifest, 2 any other failure; `ssh` exits with
//! the remote command's status.

#![forbid(unsafe_code)]

mod card;
mod cli;
mod confirm;
mod creds;
mod error;
mod run;
mod sign;
mod ssh;

use std::process::ExitCode;

use clap::Parser;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    match run::run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ahocloud: {e}");
            e.exit_code()
        }
    }
}
