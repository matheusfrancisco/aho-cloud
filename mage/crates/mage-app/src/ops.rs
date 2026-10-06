//! Operator commands: thin CLI wrappers over the server's operator entry points
//! (`mage_server::compose::{migrate, jobs_dead_*, admin_*, runner_bootstrap_token_create,
//! eval_triage_*, knowledge_eval}`), all reading `[server]`. Results go to stdout (tables as
//! tab-separated lines, reports as JSON); notes go to stderr.

use std::io::Read;

use anyhow::{Context, Result, bail};
use mage_core::ports::{Cursor, PageRequest};
use mage_infra::config::ConfigFile;
use mage_server::app::knowledge_query::eval::EvalOptions;
use mage_server::compose;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::cli::{AdminCommand, Command, DeadCommand, EvalCommand, JobsCommand, KnowledgeCommand, OrgCommand, RunnerTokenCommand, SecretCommand, SecretInput, TriageEvalCommand};

/// Runs one operator command (`command` is never `Start`).
pub async fn run(file: &ConfigFile, command: Command) -> Result<()> {
    let config: mage_server::Config = file.section("server").context("operator commands read the [server] section")?;
    match command {
        Command::Start { .. } => bail!("`mage start` is not an operator command"),
        Command::Migrate => {
            compose::migrate(&config).await?;
            eprintln!("migrations applied");
        }
        Command::Jobs { command: JobsCommand::Dead { command } } => dead(&config, command).await?,
        Command::Admin { command } => admin(&config, command).await?,
        Command::Eval { command: EvalCommand::Triage { command } } => triage(&config, command).await?,
        Command::Knowledge { command: KnowledgeCommand::Eval { org, repo, ks, max_tickets, disabled_layers } } => {
            let mut options = EvalOptions { repo, disabled_layers, ..EvalOptions::default() };
            if !ks.is_empty() {
                options.ks = ks;
            }
            if let Some(max) = max_tickets {
                options.max_tickets = max;
            }
            let report = compose::knowledge_eval(&config, &org, options).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(())
}

async fn dead(config: &mage_server::Config, command: DeadCommand) -> Result<()> {
    match command {
        DeadCommand::List { kind, limit, after } => {
            let page = PageRequest { after: after.map(Cursor), limit };
            let page = compose::jobs_dead_list(config, kind, page).await?;
            for job in &page.items {
                let org = job.org_id.map_or_else(|| "-".to_owned(), |o| o.to_string());
                let finished = job.finished_at.map_or_else(|| "-".to_owned(), rfc3339);
                let error = job.last_error.as_deref().unwrap_or("-").replace(['\n', '\t'], " ");
                println!("{}\t{}\t{org}\t{}/{}\t{finished}\t{error}", job.id, job.kind, job.attempts, job.max_attempts);
            }
            match page.next {
                Some(Cursor(next)) => eprintln!("{} dead jobs; next page: --after {next}", page.items.len()),
                None => eprintln!("{} dead jobs", page.items.len()),
            }
        }
        DeadCommand::Retry { job } => {
            compose::jobs_dead_retry(config, job).await?;
            eprintln!("{job} queued again");
        }
        DeadCommand::Discard { job } => {
            compose::jobs_dead_discard(config, job).await?;
            eprintln!("{job} discarded");
        }
    }
    Ok(())
}

async fn admin(config: &mage_server::Config, command: AdminCommand) -> Result<()> {
    let operator = operator();
    match command {
        AdminCommand::Org { command: OrgCommand::Create { slug, name, owner } } => {
            let org = compose::admin_org_create(config, &slug, &name, &owner, &operator).await?;
            println!("{}\t{}\t{}", org.id, org.slug, org.name);
        }
        AdminCommand::RunnerToken { command: RunnerTokenCommand::Create { name, orgs } } => {
            let issued = compose::runner_bootstrap_token_create(config, &name, &orgs, &operator).await?;
            println!("{}", issued.token);
            eprintln!(
                "one-time bootstrap token for runner {name:?}, valid until {}: put it in the file named by \
                 [runner.link] bootstrap_token (mode 0600); it is shown only once",
                rfc3339(issued.expires_at)
            );
        }
        AdminCommand::Secret { command: SecretCommand::Put { scope, name, source } } => {
            let value = secret_value(&source)?;
            let meta = compose::admin_secret_put(config, &scope, &name, value, &operator).await?;
            println!("{}\t{}\tv{}\t{}", meta.scope, meta.name, meta.version, meta.fingerprint);
        }
    }
    Ok(())
}

async fn triage(config: &mage_server::Config, command: TriageEvalCommand) -> Result<()> {
    match command {
        TriageEvalCommand::Run { org, corpus, variant, repeat, limit, concurrency } => {
            let variant_toml = std::fs::read_to_string(&variant).with_context(|| format!("variant file {}", variant.display()))?;
            let eval = compose::eval_triage_run(config, &org, &corpus, &variant_toml, repeat, limit, concurrency).await?;
            println!("{}\t{}\t{} cases", eval.id, eval.name, eval.cases);
        }
        TriageEvalCommand::Label { org, ticket: Some(ticket), paths } => {
            let truth = compose::eval_triage_label_manual(config, &org, ticket, paths.into_iter().collect()).await?;
            print_truth(&truth);
        }
        TriageEvalCommand::Label { org, ticket: None, .. } => {
            let labeled = compose::eval_triage_label_auto(config, &org).await?;
            for truth in &labeled {
                print_truth(truth);
            }
            eprintln!("{} tickets labeled", labeled.len());
        }
        TriageEvalCommand::Report { org, eval } => {
            let report = compose::eval_triage_report(config, &org, eval).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(())
}

fn print_truth(truth: &mage_server::ports::triage_eval::GroundTruth) {
    let paths: Vec<&str> = truth.paths.iter().map(|p| p.as_str()).collect();
    println!("{}\t{:?}\t{:?}\t{}", truth.ticket, truth.strength, truth.source, paths.join(","));
}

/// The OS user recorded as the audit actor.
fn operator() -> String {
    ["USER", "LOGNAME"].iter().find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty())).unwrap_or_else(|| "operator".to_owned())
}

/// The secret bytes from stdin or a file, one trailing newline dropped; never empty.
fn secret_value(source: &SecretInput) -> Result<Vec<u8>> {
    let mut value = Vec::new();
    match (&source.file, source.stdin) {
        (Some(path), false) => value = std::fs::read(path).with_context(|| format!("secret file {}", path.display()))?,
        (None, true) => {
            std::io::stdin().read_to_end(&mut value).context("reading the secret from stdin")?;
        }
        _ => bail!("pass exactly one of --stdin or --file"),
    }
    if value.ends_with(b"\n") {
        value.pop();
        if value.ends_with(b"\r") {
            value.pop();
        }
    }
    if value.is_empty() {
        bail!("the secret value is empty");
    }
    Ok(value)
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap_or_else(|_| at.to_string())
}
