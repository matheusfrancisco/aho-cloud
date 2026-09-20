//! The commands.

use std::io::{BufRead, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use cloud_api::client::Client;
use cloud_api::{
    AddDomain, Approve, CheckResult, CreateProject, DevicePoll, DevicePolled, DeviceStart, GrantStandingOrders, Promote, ProposeRelease,
    Reject, Release, Rollback, SetSecret,
};
use cloud_contract::Manifest;
use cloud_contract::draft::Draft;
use cloud_contract::ledger::{Provenance, Stage};
use cloud_contract::policy::StandingOrder;
use cloud_importers::Overruled;
use serde_json::json;

use crate::cli::{
    Cli, Command, DEFAULT_API_URL, DeployArgs, DomainsCmd, GrantArgs, ManifestCmd, ReleaseCmd, SecretsCmd, StandingOrdersCmd,
};
use crate::creds::{self, Credentials};
use crate::error::{Error, Result};
use crate::{archive, card, confirm, sign};

struct Ctx {
    api_url: Option<String>,
    home: Option<PathBuf>,
    project: Option<String>,
}

impl Ctx {
    fn home(&self) -> Result<PathBuf> {
        creds::home(self.home.as_deref())
    }

    /// Stored credentials and a client authenticated with them.
    fn session(&self) -> Result<(Credentials, Client)> {
        let c = creds::load(&self.home()?)?;
        let url = self.api_url.clone().unwrap_or_else(|| c.api_url.clone());
        let client = Client::new(url, Some(c.token.clone()));
        Ok((c, client))
    }

    /// `--project`, else `project` in ./deploy.json.
    fn project(&self) -> Result<String> {
        if let Some(p) = &self.project {
            return Ok(p.clone());
        }
        let missing = || Error::Invalid("no project: pass --project, or run where deploy.json is".into());
        let text = std::fs::read_to_string("deploy.json").map_err(|_| missing())?;
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| Error::Invalid(format!("deploy.json: {e}")))?;
        v.get("project").and_then(|p| p.as_str()).map(str::to_owned).ok_or_else(missing)
    }
}

pub async fn run(cli: Cli) -> Result<ExitCode> {
    let ctx = Ctx { api_url: cli.api_url, home: cli.home, project: cli.project };
    match cli.command {
        Command::Login { device_name } => login(&ctx, device_name).await,
        Command::Logout => logout(&ctx),
        Command::Whoami => whoami(&ctx).await,
        Command::Manifest(ManifestCmd::Init { dir, accept_inferred }) => manifest_init(&dir, accept_inferred),
        Command::Manifest(ManifestCmd::Check { file, edge }) => manifest_check(&file, edge.as_deref()),
        Command::Deploy(args) => deploy(&ctx, args).await,
        Command::Releases => releases(&ctx).await,
        Command::Release(ReleaseCmd::Show { release }) => release_show(&ctx, &release).await,
        Command::Approve { release, stage } => approve(&ctx, &release, stage).await,
        Command::Reject { release, stage, reason } => reject(&ctx, &release, stage, reason).await,
        Command::Promote { release, stage } => promote(&ctx, &release, stage).await,
        Command::Rollback { release } => rollback(&ctx, &release).await,
        Command::Domains(cmd) => domains(&ctx, cmd).await,
        Command::Secrets(cmd) => secrets(&ctx, cmd).await,
        Command::StandingOrders(StandingOrdersCmd::Grant(args)) => grant(&ctx, args).await,
    }
}

// ---- auth -----------------------------------------------------------------------------------

fn default_device_name() -> String {
    let user = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "user".into());
    let host = hostname::get().ok().and_then(|h| h.into_string().ok()).unwrap_or_else(|| "device".into());
    format!("{user}@{host}")
}

async fn login(ctx: &Ctx, device_name: Option<String>) -> Result<ExitCode> {
    let api = ctx.api_url.clone().unwrap_or_else(|| DEFAULT_API_URL.to_owned());
    let client = Client::new(api.clone(), None);
    let key = sign::generate();
    let start = DeviceStart { device_name: device_name.unwrap_or_else(default_device_name), public_key: sign::public_key(&key) };
    let started = client.device_start(&start).await?;
    println!("To bind this device, open {} and enter the code {}", started.verification_uri, started.user_code);
    let deadline = Instant::now() + Duration::from_secs(started.expires_in_secs.into());
    let interval = Duration::from_millis(u64::from(started.interval_secs).saturating_mul(1000).max(100));
    let expired = || Error::Invalid("the code expired before it was approved; run `ahocloud login` again".into());
    loop {
        match client.device_poll(&DevicePoll { device_code: started.device_code.clone() }).await? {
            DevicePolled::Approved { token, key_id, user } => {
                let path = creds::store(&ctx.home()?, &Credentials::new(&api, token, key_id.clone(), &key))?;
                println!("Logged in as {} ({}). Device key {key_id} saved to {}", user.email, user.name, path.display());
                return Ok(ExitCode::SUCCESS);
            }
            DevicePolled::Expired => return Err(expired()),
            DevicePolled::Pending => {}
        }
        if Instant::now() >= deadline {
            return Err(expired());
        }
        tokio::time::sleep(interval).await;
    }
}

fn logout(ctx: &Ctx) -> Result<ExitCode> {
    if creds::remove(&ctx.home()?)? {
        println!("Logged out; this device's key is deleted.");
    } else {
        println!("Not logged in.");
    }
    Ok(ExitCode::SUCCESS)
}

async fn whoami(ctx: &Ctx) -> Result<ExitCode> {
    let (creds, client) = ctx.session()?;
    let me = client.me().await?;
    println!("{} ({}) via {} · device key {}", me.user.email, me.user.name, client.base_url(), creds.key_id);
    for m in &me.orgs {
        println!("  org {}{}", m.org, if m.owner { " (owner)" } else { "" });
    }
    Ok(ExitCode::SUCCESS)
}

// ---- manifests ------------------------------------------------------------------------------

fn read_manifest(path: &Path) -> Result<Option<Manifest>> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            Manifest::from_json(&text).map(Some).map_err(|e| Error::Invalid(format!("{}: not a deploy manifest: {e}", path.display())))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io(path)(e)),
    }
}

fn write_manifest(path: &Path, m: &Manifest) -> Result<()> {
    let mut text = serde_json::to_string_pretty(m).map_err(|e| Error::Invalid(e.to_string()))?;
    text.push('\n');
    std::fs::write(path, text).map_err(Error::io(path))
}

fn edge_text(dir: &Path) -> Result<Option<String>> {
    let path = dir.join("edge.toml");
    if !path.is_file() {
        return Ok(None);
    }
    std::fs::read_to_string(&path).map(Some).map_err(Error::io(path))
}

/// The existing `deploy.json` (declared), then repo files, then the session, merged.
fn assemble(dir: &Path, session: Option<&Draft>) -> Result<(Draft, Vec<Overruled>)> {
    let mut draft = read_manifest(&dir.join("deploy.json"))?.map(|m| cloud_importers::from_manifest(&m)).unwrap_or_default();
    let scan = cloud_importers::scan(dir);
    if !scan.found.is_empty() {
        eprintln!("read {}", scan.found.join(", "));
    }
    for p in &scan.problems {
        eprintln!("note: {p}");
    }
    let mut overruled = cloud_importers::merge(&mut draft, &scan.draft);
    if let Some(s) = session {
        overruled.extend(cloud_importers::merge(&mut draft, s));
    }
    Ok((draft, overruled))
}

fn confirm_stdio(draft: &mut Draft, overruled: &[Overruled], accept_inferred: bool) -> Result<Manifest> {
    let stdin = std::io::stdin();
    let mut p = confirm::Prompter::new(stdin.lock(), std::io::stderr(), accept_inferred);
    confirm::confirm(draft, overruled, &mut p)
}

fn manifest_init(dir: &Path, accept_inferred: bool) -> Result<ExitCode> {
    let (mut draft, overruled) = assemble(dir, None)?;
    if !draft.fields.keys().any(|k| k == "/source" || k.starts_with("/source/")) {
        let a = archive::pack(dir)?;
        eprintln!("source: this directory as an archive, {} ({} files); `ahocloud deploy` uploads it", a.hash, a.files.len());
        confirm::set(&mut draft, "/source", json!({"kind": "archive", "hash": a.hash}));
    }
    let manifest = confirm_stdio(&mut draft, &overruled, accept_inferred)?;
    let path = dir.join("deploy.json");
    write_manifest(&path, &manifest)?;
    println!("wrote {}", path.display());
    let result = CheckResult::of(&manifest, edge_text(dir)?.as_deref());
    if !result.accepted() {
        print!("{}", card::check(&result));
        println!("the gate would refuse it as is; fix the fields above, then run `ahocloud manifest check`");
    }
    Ok(ExitCode::SUCCESS)
}

fn manifest_check(file: &Path, edge: Option<&Path>) -> Result<ExitCode> {
    let text = std::fs::read_to_string(file).map_err(Error::io(file))?;
    let manifest = Manifest::from_json(&text).map_err(|e| Error::Invalid(format!("{}: not a deploy manifest: {e}", file.display())))?;
    let edge_path = edge.map(Path::to_owned).or_else(|| {
        let beside = file.parent().unwrap_or(Path::new(".")).join("edge.toml");
        beside.is_file().then_some(beside)
    });
    let edge_text = edge_path.as_ref().map(|p| std::fs::read_to_string(p).map_err(Error::io(p))).transpose()?;
    let result = CheckResult::of(&manifest, edge_text.as_deref());
    if result.accepted() {
        let with = edge_path.map(|p| format!(" with {}", p.display())).unwrap_or_default();
        println!("accepted: {}{with} ({})", file.display(), manifest.hash());
        return Ok(ExitCode::SUCCESS);
    }
    print!("{}", card::check(&result));
    Err(Error::Refused)
}

// ---- deploy ---------------------------------------------------------------------------------

struct LoadedSession {
    draft: Draft,
    summary: cloud_draft::SessionSummary,
    bytes: Vec<u8>,
}

/// Newest `*.jsonl` in `DIR/.aho/sessions` by modification time (then name).
fn newest_session(dir: &Path) -> Result<Option<PathBuf>> {
    let sessions = dir.join(".aho").join("sessions");
    let entries = match std::fs::read_dir(&sessions) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(sessions)(e)),
    };
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries {
        let entry = entry.map_err(Error::io(&sessions))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") || !path.is_file() {
            continue;
        }
        let modified = entry.metadata().and_then(|m| m.modified()).map_err(Error::io(&path))?;
        if best.as_ref().is_none_or(|(t, p)| (modified, &path) > (*t, p)) {
            best = Some((modified, path));
        }
    }
    Ok(best.map(|(_, p)| p))
}

/// Reads and verifies a session log; its id is the file stem, as aho names them.
fn load_session(path: &Path) -> Result<LoadedSession> {
    let bytes = std::fs::read(path).map_err(Error::io(path))?;
    let text = std::str::from_utf8(&bytes).map_err(|_| Error::Invalid(format!("{} is not UTF-8", path.display())))?;
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Error::Invalid(format!("{} has no session id in its name", path.display())))?;
    let session_err = |source| Error::Session { path: path.to_owned(), source };
    let entries = cloud_draft::verify_jsonl(text).map_err(session_err)?;
    let summary = cloud_draft::SessionSummary::of(id, &entries).ok_or_else(|| session_err(cloud_draft::Error::Empty))?;
    let draft = cloud_draft::draft_from_entries(id, &entries);
    eprintln!("drafting from aho session {id} ({} entries, chain verified)", summary.entries);
    Ok(LoadedSession { draft, summary, bytes })
}

async fn ensure_project(client: &Client, project: &str, org: Option<&str>) -> Result<()> {
    match client.project(project).await {
        Ok(_) => Ok(()),
        Err(e) if e.code() == Some("not_found") => {
            let org = match org {
                Some(o) => o.to_owned(),
                None => {
                    let me = client.me().await?;
                    match me.orgs.as_slice() {
                        [only] => only.org.clone(),
                        [] => return Err(Error::Invalid("you belong to no org yet; create one in the console".into())),
                        many => {
                            let names: Vec<&str> = many.iter().map(|m| m.org.as_str()).collect();
                            return Err(Error::Invalid(format!(
                                "project {project} does not exist; pass --org (one of {})",
                                names.join(", ")
                            )));
                        }
                    }
                }
            };
            client.create_project(&org, &CreateProject { slug: project.to_owned() }).await?;
            println!("created project {project} in org {org}");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

async fn deploy(ctx: &Ctx, args: DeployArgs) -> Result<ExitCode> {
    let (_, client) = ctx.session()?;
    let dir = args.dir.as_path();
    let session_path = match &args.session {
        Some(p) => Some(p.clone()),
        None => newest_session(dir)?,
    };
    let session = session_path.as_deref().map(load_session).transpose()?;
    if session.is_none() {
        eprintln!("no aho session in {}; the release will be unattested", dir.join(".aho/sessions").display());
    }
    let (mut draft, overruled) = assemble(dir, session.as_ref().map(|s| &s.draft))?;
    let archive = archive::pack(dir)?;
    for f in archive.env_files() {
        eprintln!("warning: {f} is not gitignored, so it is uploaded with the source");
    }
    confirm::set(&mut draft, "/source", json!({"kind": "archive", "hash": archive.hash}));
    let manifest = confirm_stdio(&mut draft, &overruled, args.accept_inferred)?;
    if let Some(p) = &ctx.project
        && *p != manifest.project
    {
        return Err(Error::Invalid(format!("--project {p} is not the manifest's project `{}`", manifest.project)));
    }
    write_manifest(&dir.join("deploy.json"), &manifest)?;
    let edge = edge_text(dir)?;
    let check = CheckResult::of(&manifest, edge.as_deref());
    if !check.accepted() {
        print!("{}", card::check(&check));
        return Err(Error::Refused);
    }

    let project = manifest.project.clone();
    ensure_project(&client, &project, args.org.as_deref()).await?;
    let files = archive.files.len();
    let uploaded = client.upload_archive(&project, archive.bytes).await?;
    if uploaded.hash != archive.hash {
        return Err(Error::Invalid(format!(
            "the cloud stored {} but the archive is {}; nothing was proposed",
            uploaded.hash, archive.hash
        )));
    }
    println!("uploaded source {} ({files} files, {} bytes)", uploaded.hash, uploaded.bytes);
    let provenance = match session {
        Some(s) => {
            let up = client.upload_session(&project, &s.summary.session_id, s.bytes).await?;
            if up.session_id != s.summary.session_id || up.head_hash != s.summary.head_hash {
                return Err(Error::Invalid(format!(
                    "the cloud verified session {} up to {}, but the local log is {} up to {}; nothing was proposed",
                    up.session_id, up.head_hash, s.summary.session_id, s.summary.head_hash
                )));
            }
            println!("uploaded session {} ({} entries, head {})", up.session_id, up.entries, up.head_hash);
            Provenance::Attested { session_id: up.session_id, head_hash: up.head_hash }
        }
        None => Provenance::Unattested { commit: None, ci_run: None },
    };
    let release = client.propose(&project, &ProposeRelease { manifest, edge_toml: edge, provenance }).await?;
    println!("proposed release {}", release.id);
    print!("{}", card::release(&release));
    println!("next: `ahocloud promote {0} --stage preview` once built, then `ahocloud approve {0} --stage staging`", release.id);
    Ok(ExitCode::SUCCESS)
}

// ---- releases -------------------------------------------------------------------------------

async fn releases(ctx: &Ctx) -> Result<ExitCode> {
    let (_, client) = ctx.session()?;
    let list = client.releases(&ctx.project()?).await?;
    if list.is_empty() {
        println!("no releases yet");
    }
    for r in &list {
        println!("{}", card::release_row(r));
    }
    Ok(ExitCode::SUCCESS)
}

async fn release_show(ctx: &Ctx, release: &str) -> Result<ExitCode> {
    let (_, client) = ctx.session()?;
    print!("{}", card::release(&client.release(&ctx.project()?, release).await?));
    Ok(ExitCode::SUCCESS)
}

/// Fetches a release to sign for, refusing one whose manifest does not hash to the hash that
/// would be signed: the card shown must be the manifest approved.
async fn release_to_sign(client: &Client, project: &str, release: &str) -> Result<Release> {
    let r = client.release(project, release).await?;
    if r.project != project || r.id != release {
        return Err(Error::Invalid(format!(
            "asked for {project}/{release}, the server answered with {}/{}; refusing to sign",
            r.project, r.id
        )));
    }
    let computed = r.manifest.hash();
    if computed != r.manifest_hash {
        return Err(Error::Invalid(format!(
            "refusing to sign: the release's manifest hashes to {computed}, not to the {} the server says",
            r.manifest_hash
        )));
    }
    Ok(r)
}

async fn approve(ctx: &Ctx, release: &str, stage: Stage) -> Result<ExitCode> {
    let (creds, client) = ctx.session()?;
    let project = ctx.project()?;
    let r = release_to_sign(&client, &project, release).await?;
    print!("{}", card::release(&r));
    let proof = sign::approval(&creds, &project, release, &r.manifest_hash, stage)?;
    let after = client.approve(&project, release, &Approve { stage, proof }).await?;
    let n = after.approvals.iter().filter(|a| a.stage == stage).count();
    println!(
        "approved {release} for {}: {n} approval(s) for {} so far; {}",
        stage.as_str(),
        stage.as_str(),
        card::requirement(&after.requirement)
    );
    Ok(ExitCode::SUCCESS)
}

async fn reject(ctx: &Ctx, release: &str, stage: Stage, reason: String) -> Result<ExitCode> {
    let (_, client) = ctx.session()?;
    let after = client.reject(&ctx.project()?, release, &Reject { stage, reason }).await?;
    println!("{}", card::release_row(&after));
    Ok(ExitCode::SUCCESS)
}

async fn promote(ctx: &Ctx, release: &str, stage: Stage) -> Result<ExitCode> {
    let (_, client) = ctx.session()?;
    let after = client.promote(&ctx.project()?, release, &Promote { stage }).await?;
    println!("{}", card::release_row(&after));
    Ok(ExitCode::SUCCESS)
}

async fn rollback(ctx: &Ctx, target: &str) -> Result<ExitCode> {
    let (creds, client) = ctx.session()?;
    let project = ctx.project()?;
    let to = release_to_sign(&client, &project, target).await?;
    let live_id = client.project(&project).await?.active.into_iter().find(|h| h.stage == Stage::Live).map(|h| h.release);
    let live = match &live_id {
        Some(id) => Some(client.release(&project, id).await?),
        None => None,
    };
    let diff = cloud_contract::diff::diff(live.as_ref().map(|r| &r.manifest), &to.manifest);
    println!("rolling back to {target}; against live ({}):", live_id.as_deref().unwrap_or("nothing live"));
    print!("{}", card::diff(&diff));
    let proof = if diff.is_widening() {
        println!("{target} widens the live surface; signing with this device's key");
        Some(sign::approval(&creds, &project, target, &to.manifest_hash, Stage::Live)?)
    } else {
        None
    };
    let after = client.rollback(&project, &Rollback { release: target.to_owned(), proof }).await?;
    println!("{}", card::release_row(&after));
    Ok(ExitCode::SUCCESS)
}

async fn grant(ctx: &Ctx, args: GrantArgs) -> Result<ExitCode> {
    let (creds, client) = ctx.session()?;
    let project = ctx.project()?;
    let mut orders = Vec::new();
    if args.restart {
        orders.push(StandingOrder::Restart);
    }
    if let Some((min, max)) = args.scale {
        orders.push(StandingOrder::Scale { min, max });
    }
    if args.maintenance_page {
        orders.push(StandingOrder::MaintenancePage);
    }
    if args.rollback_within_ceiling {
        orders.push(StandingOrder::RollbackWithinCeiling);
    }
    if orders.is_empty() {
        return Err(Error::Invalid(
            "grant at least one of --restart, --scale MIN:MAX, --maintenance-page, --rollback-within-ceiling".into(),
        ));
    }
    let proof = sign::standing_orders(&creds, &project, &orders)?;
    let n = orders.len();
    client.grant_standing_orders(&project, &GrantStandingOrders { orders, ttl_secs: args.ttl, proof }).await?;
    println!("granted {n} standing order(s) on {project} for {}s", args.ttl);
    Ok(ExitCode::SUCCESS)
}

// ---- domains and secrets --------------------------------------------------------------------

async fn domains(ctx: &Ctx, cmd: DomainsCmd) -> Result<ExitCode> {
    let (_, client) = ctx.session()?;
    let project = ctx.project()?;
    match cmd {
        DomainsCmd::Add { hostname, mode } => {
            let d = client.add_domain(&project, &AddDomain { hostname: hostname.clone(), mode: mode.into() }).await?;
            print!("{}", card::domain(&d));
            println!("create the records above, then run `ahocloud domains verify {hostname}`");
        }
        DomainsCmd::List => {
            for d in client.domains(&project).await? {
                print!("{}", card::domain(&d));
            }
        }
        DomainsCmd::Verify { hostname } => print!("{}", card::domain(&client.verify_domain(&project, &hostname).await?)),
    }
    Ok(ExitCode::SUCCESS)
}

async fn secrets(ctx: &Ctx, cmd: SecretsCmd) -> Result<ExitCode> {
    let (_, client) = ctx.session()?;
    let project = ctx.project()?;
    match cmd {
        SecretsCmd::Set { name, stage } => {
            let value = read_secret(&name)?;
            let view = client.set_secret(&project, &SetSecret { name: name.clone(), stage, value }).await?;
            println!("set {name} for {} (fingerprint {})", stage.as_str(), view.fingerprint);
        }
        SecretsCmd::List => {
            for s in client.secrets(&project).await? {
                let stages: Vec<&str> = s.stages.iter().map(|s| s.as_str()).collect();
                println!("{}  {}  {}", s.name, stages.join(","), s.fingerprint);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The value comes from stdin only: piped (all of it, minus one trailing newline), or typed at
/// a terminal with echo off.
fn read_secret(name: &str) -> Result<String> {
    let stdin = std::io::stdin();
    let raw = if stdin.is_terminal() {
        eprint!("value for {name} (not echoed): ");
        let line = read_line_quietly()?;
        eprintln!();
        line
    } else {
        let mut s = String::new();
        stdin.lock().read_to_string(&mut s).map_err(Error::io("<stdin>"))?;
        s
    };
    let value = raw.strip_suffix('\n').map_or(raw.as_str(), |v| v.strip_suffix('\r').unwrap_or(v));
    if value.is_empty() {
        return Err(Error::Invalid(format!(
            "no value for {name} on stdin; pipe it in, e.g. `ahocloud secrets set {name} --stage live < value.txt`"
        )));
    }
    Ok(value.to_owned())
}

#[cfg(unix)]
fn read_line_quietly() -> Result<String> {
    use rustix::termios::{LocalModes, OptionalActions, tcgetattr, tcsetattr};
    let stdin = std::io::stdin();
    let io = |e: rustix::io::Errno| Error::io("<stdin>")(e.into());
    let saved = tcgetattr(&stdin).map_err(io)?;
    let mut quiet = saved.clone();
    quiet.local_modes.remove(LocalModes::ECHO);
    tcsetattr(&stdin, OptionalActions::Now, &quiet).map_err(io)?;
    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line);
    tcsetattr(&stdin, OptionalActions::Now, &saved).map_err(io)?;
    read.map_err(Error::io("<stdin>"))?;
    Ok(line)
}

#[cfg(not(unix))]
fn read_line_quietly() -> Result<String> {
    eprint!("(this terminal cannot hide input) ");
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).map_err(Error::io("<stdin>"))?;
    Ok(line)
}
