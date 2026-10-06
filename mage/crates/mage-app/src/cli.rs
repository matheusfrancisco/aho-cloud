//! Command line of the `mage` binary (README "Crates": `mage start server | indexer | runner |
//! all` plus the operator commands).

use std::fmt::Display;
use std::path::PathBuf;
use std::str::FromStr;

use clap::{Args, Parser, Subcommand, ValueEnum};
use mage_core::domain::{JobId, JobKind, Layer, RepoId, RepoPath, TicketId, TriageEvalId};

#[derive(Debug, Parser)]
#[command(name = "mage", version, about = "ahomage: server, indexer and runner roles plus operator commands")]
pub struct Cli {
    /// TOML config file; `MAGE_<SECTION>__<KEY>=value` variables override its keys.
    #[arg(long, short = 'c', global = true, env = "MAGE_CONFIG", value_name = "PATH")]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run a role until SIGINT / SIGTERM.
    Start {
        #[arg(value_enum)]
        role: StartTarget,
    },
    /// Apply the database migrations (uses `[server.database]`).
    Migrate,
    /// Job queue operations.
    Jobs {
        #[command(subcommand)]
        command: JobsCommand,
    },
    /// Orgs, runner tokens and platform secrets.
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
    /// Offline evaluations.
    Eval {
        #[command(subcommand)]
        command: EvalCommand,
    },
    /// Knowledge tools.
    Knowledge {
        #[command(subcommand)]
        command: KnowledgeCommand,
    },
}

/// What `mage start` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum StartTarget {
    Server,
    Indexer,
    Runner,
    /// Every role as tasks of one process (development only).
    All,
}

#[derive(Debug, Subcommand)]
pub enum JobsCommand {
    /// Dead-lettered jobs.
    Dead {
        #[command(subcommand)]
        command: DeadCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum DeadCommand {
    /// List dead jobs, newest first.
    List {
        /// Only this kind (e.g. `index.fetch`).
        #[arg(long, value_parser = parse::<JobKind>)]
        kind: Option<JobKind>,
        #[arg(long, default_value_t = 50)]
        limit: u16,
        /// Cursor printed after the previous page.
        #[arg(long)]
        after: Option<String>,
    },
    /// Queue a dead job again.
    Retry {
        #[arg(value_parser = parse::<JobId>)]
        job: JobId,
    },
    /// Cancel a dead job for good.
    Discard {
        #[arg(value_parser = parse::<JobId>)]
        job: JobId,
    },
}

#[derive(Debug, Subcommand)]
pub enum AdminCommand {
    /// Organizations.
    Org {
        #[command(subcommand)]
        command: OrgCommand,
    },
    /// One-time runner bootstrap tokens.
    RunnerToken {
        #[command(subcommand)]
        command: RunnerTokenCommand,
    },
    /// Vault secrets.
    Secret {
        #[command(subcommand)]
        command: SecretCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum OrgCommand {
    /// Create an org with one owner.
    Create {
        /// Immutable URL slug.
        slug: String,
        #[arg(long)]
        name: String,
        /// GitHub login of the first owner.
        #[arg(long)]
        owner: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum RunnerTokenCommand {
    /// Issue a 24 h one-time `mgb_` token; prints the token alone on stdout.
    Create {
        /// Runner name (`[runner.link] name`).
        name: String,
        /// Restrict the runner to these orgs (repeatable); none = shared runner.
        #[arg(long = "org", value_name = "SLUG")]
        orgs: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum SecretCommand {
    /// Store a new secret version; the value comes from stdin or a file, never argv.
    Put {
        /// `platform` or `org:<slug>`.
        scope: String,
        /// Secret name, e.g. `github/oauth_client_secret`.
        name: String,
        #[command(flatten)]
        source: SecretInput,
    },
}

#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
pub struct SecretInput {
    /// Read the value from stdin (one trailing newline is dropped).
    #[arg(long)]
    pub stdin: bool,
    /// Read the value from this file (one trailing newline is dropped).
    #[arg(long, value_name = "PATH")]
    pub file: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum EvalCommand {
    /// Triage evaluation (triage.md "Evaluation").
    Triage {
        #[command(subcommand)]
        command: TriageEvalCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum TriageEvalCommand {
    /// Start an evaluation over a labeled corpus.
    Run {
        #[arg(long)]
        org: String,
        /// Corpus name.
        #[arg(long)]
        corpus: String,
        /// Variant TOML file (model, prompt and knowledge settings under test).
        #[arg(long, value_name = "PATH")]
        variant: PathBuf,
        #[arg(long, default_value_t = 1)]
        repeat: u8,
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long)]
        concurrency: Option<u16>,
    },
    /// Label ground truth: automatically from resolved tickets, or one ticket by hand.
    Label {
        #[arg(long)]
        org: String,
        /// Label this ticket by hand (with `--path`) instead of auto-labeling.
        #[arg(long, value_parser = parse::<TicketId>, requires = "paths")]
        ticket: Option<TicketId>,
        /// Repo path the cause lives in (repeatable).
        #[arg(long = "path", value_name = "PATH", value_parser = parse::<RepoPath>, requires = "ticket")]
        paths: Vec<RepoPath>,
    },
    /// Print an evaluation's report as JSON.
    Report {
        #[arg(long)]
        org: String,
        #[arg(value_parser = parse::<TriageEvalId>)]
        eval: TriageEvalId,
    },
}

#[derive(Debug, Subcommand)]
pub enum KnowledgeCommand {
    /// Replay resolved tickets against Knowledge retrieval; prints the report as JSON.
    Eval {
        #[arg(long)]
        org: String,
        #[arg(long, value_parser = parse::<RepoId>)]
        repo: Option<RepoId>,
        /// Recall cut-offs (repeatable; default 1, 5, 10, 30).
        #[arg(long = "k")]
        ks: Vec<usize>,
        #[arg(long)]
        max_tickets: Option<usize>,
        /// Ablate a layer (repeatable), e.g. `app_map`.
        #[arg(long = "disable-layer", value_parser = parse_layer)]
        disabled_layers: Vec<Layer>,
    },
}

fn parse<T>(s: &str) -> Result<T, String>
where
    T: FromStr,
    T::Err: Display,
{
    s.parse().map_err(|e: T::Err| e.to_string())
}

fn parse_layer(s: &str) -> Result<Layer, String> {
    serde_json::from_value(serde_json::Value::String(s.to_owned())).map_err(|_| {
        let names: Vec<String> = Layer::ALL.iter().filter_map(|l| serde_json::to_value(l).ok()).filter_map(|v| v.as_str().map(str::to_owned)).collect();
        format!("unknown layer {s:?}; one of {}", names.join(", "))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn cli(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("mage").chain(args.iter().copied()))
    }

    #[test]
    fn command_tree_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn start_takes_exactly_one_known_role() {
        for (arg, role) in [("server", StartTarget::Server), ("indexer", StartTarget::Indexer), ("runner", StartTarget::Runner), ("all", StartTarget::All)] {
            let parsed = cli(&["--config", "m.toml", "start", arg]).unwrap();
            assert!(matches!(parsed.command, Command::Start { role: r } if r == role), "{arg}");
            assert_eq!(parsed.config, Some(PathBuf::from("m.toml")));
        }
        assert!(cli(&["start", "web"]).is_err());
        assert!(cli(&["start"]).is_err());
        // `--config` is global: accepted after the subcommand too.
        assert_eq!(cli(&["start", "all", "-c", "x.toml"]).unwrap().config, Some(PathBuf::from("x.toml")));
    }

    #[test]
    fn jobs_dead_commands_parse_typed_ids_and_kinds() {
        let job = JobId::from(uuid_v7());
        match cli(&["jobs", "dead", "retry", &job.to_string()]).unwrap().command {
            Command::Jobs { command: JobsCommand::Dead { command: DeadCommand::Retry { job: parsed } } } => assert_eq!(parsed, job),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            cli(&["jobs", "dead", "discard", &job.to_string()]).unwrap().command,
            Command::Jobs { command: JobsCommand::Dead { command: DeadCommand::Discard { .. } } }
        ));
        match cli(&["jobs", "dead", "list", "--kind", "index.fetch", "--limit", "10"]).unwrap().command {
            Command::Jobs { command: JobsCommand::Dead { command: DeadCommand::List { kind, limit, after } } } => {
                assert_eq!(kind, Some(JobKind::IndexFetch));
                assert_eq!(limit, 10);
                assert_eq!(after, None);
            }
            other => panic!("{other:?}"),
        }
        assert!(cli(&["jobs", "dead", "retry", "not-a-job-id"]).is_err());
        assert!(cli(&["jobs", "dead", "list", "--kind", "no.such.kind"]).is_err());
    }

    #[test]
    fn admin_commands_parse() {
        match cli(&["admin", "org", "create", "hoop", "--name", "Hoop", "--owner", "octocat"]).unwrap().command {
            Command::Admin { command: AdminCommand::Org { command: OrgCommand::Create { slug, name, owner } } } => {
                assert_eq!((slug.as_str(), name.as_str(), owner.as_str()), ("hoop", "Hoop", "octocat"));
            }
            other => panic!("{other:?}"),
        }
        assert!(cli(&["admin", "org", "create", "hoop", "--name", "Hoop"]).is_err());
        match cli(&["admin", "runner-token", "create", "r1", "--org", "a", "--org", "b"]).unwrap().command {
            Command::Admin { command: AdminCommand::RunnerToken { command: RunnerTokenCommand::Create { name, orgs } } } => {
                assert_eq!(name, "r1");
                assert_eq!(orgs, ["a", "b"]);
            }
            other => panic!("{other:?}"),
        }
        assert!(cli(&["admin", "secret", "put", "platform", "csrf", "--stdin"]).is_ok());
        assert!(cli(&["admin", "secret", "put", "platform", "csrf", "--file", "/k"]).is_ok());
        // The value never comes from argv: exactly one of --stdin / --file.
        assert!(cli(&["admin", "secret", "put", "platform", "csrf"]).is_err());
        assert!(cli(&["admin", "secret", "put", "platform", "csrf", "--stdin", "--file", "/k"]).is_err());
    }

    #[test]
    fn eval_and_knowledge_commands_parse() {
        let run = cli(&["eval", "triage", "run", "--org", "hoop", "--corpus", "c1", "--variant", "v.toml", "--repeat", "3"]).unwrap();
        assert!(matches!(run.command, Command::Eval { command: EvalCommand::Triage { command: TriageEvalCommand::Run { repeat: 3, .. } } }));
        assert!(matches!(
            cli(&["eval", "triage", "label", "--org", "hoop"]).unwrap().command,
            Command::Eval { command: EvalCommand::Triage { command: TriageEvalCommand::Label { ticket: None, .. } } }
        ));
        let ticket = TicketId::from(uuid_v7());
        let manual = cli(&["eval", "triage", "label", "--org", "hoop", "--ticket", &ticket.to_string(), "--path", "src/a.rs", "--path", "src/b.rs"]);
        match manual.unwrap().command {
            Command::Eval { command: EvalCommand::Triage { command: TriageEvalCommand::Label { ticket: Some(t), paths, .. } } } => {
                assert_eq!(t, ticket);
                assert_eq!(paths.len(), 2);
            }
            other => panic!("{other:?}"),
        }
        // A manual label needs both the ticket and its paths.
        assert!(cli(&["eval", "triage", "label", "--org", "hoop", "--ticket", &ticket.to_string()]).is_err());
        let eval = TriageEvalId::from(uuid_v7());
        assert!(cli(&["eval", "triage", "report", "--org", "hoop", &eval.to_string()]).is_ok());
        match cli(&["knowledge", "eval", "--org", "hoop", "--k", "5", "--disable-layer", "app_map"]).unwrap().command {
            Command::Knowledge { command: KnowledgeCommand::Eval { ks, disabled_layers, repo: None, .. } } => {
                assert_eq!(ks, [5]);
                assert_eq!(disabled_layers, [Layer::AppMap]);
            }
            other => panic!("{other:?}"),
        }
        assert!(cli(&["knowledge", "eval", "--org", "hoop", "--disable-layer", "bogus"]).is_err());
    }

    fn uuid_v7() -> uuid::Uuid {
        uuid::Uuid::now_v7()
    }
}
