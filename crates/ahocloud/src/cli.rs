//! Command line. Secret values are never flags or arguments: `secrets set` and `sandbox env set`
//! read stdin.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use cloud_contract::ledger::Stage;
use cloud_contract::manifest::DomainMode;

/// Used by `login` when neither `--api-url` nor `AHOCLOUD_API_URL` is set.
pub const DEFAULT_API_URL: &str = "https://api.aho.cloud";

#[derive(Parser, Debug)]
#[command(name = "ahocloud", version, about = "Deploy to aho cloud: draft and check a manifest, propose releases, approve them.")]
pub struct Cli {
    /// Control-plane URL. Defaults to the one stored at login (login itself: https://api.aho.cloud).
    #[arg(long, env = "AHOCLOUD_API_URL", global = true)]
    pub api_url: Option<String>,
    /// Directory holding `credentials.json` [default: ~/.config/ahocloud].
    #[arg(long, env = "AHOCLOUD_HOME", global = true)]
    pub home: Option<PathBuf>,
    /// Project slug [default: `project` in ./deploy.json].
    #[arg(long, short, env = "AHOCLOUD_PROJECT", global = true)]
    pub project: Option<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Bind this device: device-code sign-in plus a new Ed25519 key that signs your approvals.
    Login {
        /// Shown on the approval screen [default: <user>@<host>].
        #[arg(long)]
        device_name: Option<String>,
    },
    /// Forget this device's credentials.
    Logout,
    /// Who the stored credentials belong to.
    Whoami,
    #[command(subcommand)]
    Manifest(ManifestCmd),
    /// Draft from the newest aho session and repo files, confirm, upload source and session,
    /// and propose a release.
    Deploy(DeployArgs),
    /// This project's releases.
    Releases,
    #[command(subcommand)]
    Release(ReleaseCmd),
    /// Sign an approval of a release for a stage with this device's key.
    Approve {
        release: String,
        #[arg(long, value_parser = parse_stage)]
        stage: Stage,
    },
    /// Reject a release for a stage.
    Reject {
        release: String,
        #[arg(long, value_parser = parse_stage)]
        stage: Stage,
        #[arg(long)]
        reason: String,
    },
    /// Activate an approved release at a stage.
    Promote {
        release: String,
        #[arg(long, value_parser = parse_stage)]
        stage: Stage,
    },
    /// Make an earlier release live again; signed when it widens the live surface.
    Rollback { release: String },
    #[command(subcommand)]
    Domains(DomainsCmd),
    #[command(subcommand)]
    Secrets(SecretsCmd),
    #[command(subcommand)]
    StandingOrders(StandingOrdersCmd),
    /// Long-lived VMs with aho installed and a persistent `/workspace`.
    #[command(subcommand)]
    Sandbox(SandboxCmd),
    /// A terminal in a sandbox: a login shell, or COMMAND (exits with its status). Gets a PTY when
    /// stdin is a terminal; otherwise plain pipes, so output is byte-faithful.
    Ssh {
        sandbox: String,
        /// Force a PTY even when stdin is not a terminal.
        #[arg(short = 't', long = "pty", conflicts_with = "no_pty")]
        pty: bool,
        /// Disable the PTY: plain stdin, stdout and stderr pipes.
        #[arg(short = 'T', long = "no-pty")]
        no_pty: bool,
        #[arg(last = true)]
        command: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum SandboxCmd {
    /// Create and boot a sandbox.
    Create {
        slug: String,
        /// Org to create it in [default: your only org].
        #[arg(long)]
        org: Option<String>,
        /// Port inside the VM served at https://<slug>.<preview domain>, to org members only.
        #[arg(long)]
        port: Option<u16>,
        /// A host the VM may reach (repeat; `*.example.com` wildcards) [default: model providers,
        /// package registries, git hosting].
        #[arg(long = "egress", value_name = "HOST")]
        egress: Vec<String>,
        /// Stop the VM this long after each start (its disk is kept) [default: 8].
        #[arg(long)]
        auto_stop_hours: Option<u16>,
    },
    /// The sandboxes of an org.
    #[command(alias = "list")]
    Ls {
        /// [default: your only org]
        #[arg(long)]
        org: Option<String>,
    },
    /// Boot a stopped sandbox with its disk and env.
    Start { slug: String },
    /// Stop the VM, keep the disk.
    Stop { slug: String },
    /// Destroy a sandbox and its disk.
    Rm {
        slug: String,
        /// Do not ask for confirmation.
        #[arg(long, short)]
        yes: bool,
    },
    /// Show the settings, or change them (a running sandbox restarts with them).
    Settings(SettingsArgs),
    #[command(subcommand)]
    Env(EnvCmd),
}

#[derive(Args, Debug)]
pub struct SettingsArgs {
    pub slug: String,
    /// Serve this port inside the VM at the sandbox URL.
    #[arg(long, conflicts_with = "no_port")]
    pub port: Option<u16>,
    /// Stop serving a port.
    #[arg(long)]
    pub no_port: bool,
    /// Replace the egress allowlist (repeat).
    #[arg(long = "egress", value_name = "HOST", conflicts_with_all = ["allow", "deny"])]
    pub egress: Vec<String>,
    /// Add a host to the egress allowlist (repeat).
    #[arg(long, value_name = "HOST")]
    pub allow: Vec<String>,
    /// Remove a host from the egress allowlist (repeat).
    #[arg(long, value_name = "HOST")]
    pub deny: Vec<String>,
    #[arg(long)]
    pub auto_stop_hours: Option<u16>,
}

impl SettingsArgs {
    pub fn changes_anything(&self) -> bool {
        self.port.is_some()
            || self.no_port
            || !self.egress.is_empty()
            || !self.allow.is_empty()
            || !self.deny.is_empty()
            || self.auto_stop_hours.is_some()
    }
}

#[derive(Subcommand, Debug)]
pub enum EnvCmd {
    /// Set a variable; it reaches the VM at its next start. The value is read from stdin, never
    /// from arguments.
    Set {
        slug: String,
        name: String,
    },
    #[command(alias = "list")]
    Ls {
        slug: String,
    },
    Rm {
        slug: String,
        name: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum ManifestCmd {
    /// Draft `deploy.json` from repo files; confirm or correct each inferred field.
    Init {
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Accept every inferred value without asking (required values are still asked for).
        #[arg(long)]
        accept_inferred: bool,
    },
    /// Run the gate's rules locally. Exit 1 when it would refuse.
    Check {
        #[arg(default_value = "deploy.json")]
        file: PathBuf,
        /// `edge.toml` to check with the manifest [default: edge.toml beside FILE, if present].
        #[arg(long)]
        edge: Option<PathBuf>,
    },
}

#[derive(Args, Debug)]
pub struct DeployArgs {
    #[arg(long, default_value = ".")]
    pub dir: PathBuf,
    /// aho session log to attest [default: newest DIR/.aho/sessions/*.jsonl].
    #[arg(long)]
    pub session: Option<PathBuf>,
    /// Accept every inferred value without asking.
    #[arg(long)]
    pub accept_inferred: bool,
    /// Org to create the project in if it does not exist [default: your only org].
    #[arg(long)]
    pub org: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum ReleaseCmd {
    /// The approval card: what the release widens and narrows, and what approving it takes.
    Show { release: String },
}

#[derive(Subcommand, Debug)]
pub enum DomainsCmd {
    /// Bind a hostname; prints the DNS records to create.
    Add {
        hostname: String,
        #[arg(long, value_enum, default_value_t = Mode::Cname)]
        mode: Mode,
    },
    List,
    /// Check the DNS records now.
    Verify {
        hostname: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum SecretsCmd {
    /// Set a secret's value for a stage. The value is read from stdin, never from arguments.
    Set {
        name: String,
        #[arg(long, value_parser = parse_stage)]
        stage: Stage,
    },
    List,
}

#[derive(Subcommand, Debug)]
pub enum StandingOrdersCmd {
    /// Pre-approve bounded on-call actions, signed with this device's key.
    Grant(GrantArgs),
}

#[derive(Args, Debug)]
pub struct GrantArgs {
    #[arg(long)]
    pub restart: bool,
    /// Replica range, `MIN:MAX`.
    #[arg(long, value_parser = parse_scale)]
    pub scale: Option<(u16, u16)>,
    #[arg(long)]
    pub maintenance_page: bool,
    /// Roll back to any earlier release that fits within the live one.
    #[arg(long)]
    pub rollback_within_ceiling: bool,
    /// How long the grant lasts: `90s`, `30m`, `12h`, `7d`, or seconds.
    #[arg(long, value_parser = parse_duration)]
    pub ttl: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    Cname,
    Apex,
    NsDelegated,
}

impl From<Mode> for DomainMode {
    fn from(m: Mode) -> Self {
        match m {
            Mode::Cname => DomainMode::Cname,
            Mode::Apex => DomainMode::Apex,
            Mode::NsDelegated => DomainMode::NsDelegated,
        }
    }
}

pub fn parse_stage(s: &str) -> Result<Stage, String> {
    serde_json::from_value(serde_json::Value::String(s.to_ascii_lowercase()))
        .map_err(|_| format!("`{s}` is not a stage (preview, staging, live)"))
}

fn parse_scale(s: &str) -> Result<(u16, u16), String> {
    let (a, b) = s.split_once(':').ok_or("expected MIN:MAX")?;
    let (min, max) = (a.parse::<u16>().map_err(|e| e.to_string())?, b.parse::<u16>().map_err(|e| e.to_string())?);
    if min > max {
        return Err(format!("{min} is above {max}"));
    }
    Ok((min, max))
}

pub fn parse_duration(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let n: u64 = s[..split].parse().map_err(|_| format!("`{s}` is not a duration"))?;
    let unit = match &s[split..] {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        other => return Err(format!("unknown unit `{other}` (use s, m, h, d)")),
    };
    n.checked_mul(unit).filter(|v| *v > 0).ok_or_else(|| format!("`{s}` is not a positive duration"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90"), Ok(90));
        assert_eq!(parse_duration("30m"), Ok(1800));
        assert_eq!(parse_duration("7d"), Ok(604_800));
        assert!(parse_duration("0h").is_err());
        assert!(parse_duration("1w").is_err());
    }

    #[test]
    fn scales_and_stages() {
        assert_eq!(parse_scale("1:4"), Ok((1, 4)));
        assert!(parse_scale("4:1").is_err());
        assert_eq!(parse_stage("Live"), Ok(Stage::Live));
        assert!(parse_stage("prod").is_err());
    }

    #[test]
    fn secret_values_have_no_argument() {
        assert!(Cli::try_parse_from(["ahocloud", "secrets", "set", "K", "--stage", "live", "value"]).is_err());
        assert!(Cli::try_parse_from(["ahocloud", "secrets", "set", "K", "--stage", "live"]).is_ok());
        assert!(Cli::try_parse_from(["ahocloud", "sandbox", "env", "set", "box", "K", "value"]).is_err());
        assert!(Cli::try_parse_from(["ahocloud", "sandbox", "env", "set", "box", "K"]).is_ok());
    }
}
