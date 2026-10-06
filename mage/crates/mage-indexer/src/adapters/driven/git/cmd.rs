//! Hardened `git` invocations (knowledge.md "Adapter internals"): cleared environment except
//! `PATH` and a dedicated empty `HOME`, no system/global config, no prompts, no hooks, HTTPS-only
//! transport, no credential helpers. Credentials travel only through `GIT_CONFIG_*` environment
//! variables of the network commands (never argv, the mirror config, logs or disk).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use mage_core::ports::source_host::FetchSpec;
use tokio::process::Command;

use crate::ports::git::GitError;

/// Per-process invocation settings shared by every command.
#[derive(Debug, Clone)]
pub(super) struct GitEnv {
    pub bin: PathBuf,
    pub home: PathBuf,
    pub path: Option<OsString>,
    pub allow_file_protocol: bool,
}

/// `http.<scheme>://<host>[:port]/.extraheader` = `AUTHORIZATION: basic base64(user:token)`.
pub(super) struct Credentials {
    key: String,
    value: String,
}

impl Credentials {
    pub fn from_spec(spec: &FetchSpec) -> Credentials {
        let url = &spec.url;
        let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
        let key = format!("http.{}://{}{}/.extraheader", url.scheme(), url.host_str().unwrap_or_default(), port);
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", spec.username, spec.token.token.expose()));
        Credentials { key, value: format!("AUTHORIZATION: basic {basic}") }
    }
}

/// The remote URL as stored in the mirror config and passed in argv: credentials stripped.
pub(super) fn remote_url(spec: &FetchSpec) -> Result<String, GitError> {
    let mut url = spec.url.clone();
    if !url.username().is_empty() {
        url.set_username("").map_err(|()| GitError::Invalid("fetch url cannot carry a username".into()))?;
    }
    if url.password().is_some() {
        url.set_password(None).map_err(|()| GitError::Invalid("fetch url cannot carry a password".into()))?;
    }
    Ok(url.into())
}

impl GitEnv {
    /// `git --git-dir=<dir> -c … <args>` with the hardened environment. `creds` only for
    /// commands that talk to the remote.
    pub fn command<I, S>(&self, git_dir: Option<&Path>, args: I, creds: Option<&Credentials>) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new(&self.bin);
        cmd.env_clear();
        if let Some(path) = &self.path {
            cmd.env("PATH", path);
        }
        cmd.env("HOME", &self.home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0");
        match creds {
            Some(creds) => {
                cmd.env("GIT_CONFIG_COUNT", "1").env("GIT_CONFIG_KEY_0", &creds.key).env("GIT_CONFIG_VALUE_0", &creds.value);
            }
            None => {
                cmd.env("GIT_CONFIG_COUNT", "0");
            }
        }
        if let Some(dir) = git_dir {
            let mut flag = OsString::from("--git-dir=");
            flag.push(dir);
            cmd.arg(flag);
        }
        cmd.args(["-c", "core.hooksPath=/dev/null", "-c", "protocol.allow=never", "-c", "protocol.https.allow=always", "-c", "credential.helper="]);
        if self.allow_file_protocol {
            cmd.args(["-c", "protocol.file.allow=always"]);
        }
        // Auto gc/maintenance (after fetch) runs in the foreground, inside the command timeout.
        cmd.args(["-c", "gc.autoDetach=false", "-c", "maintenance.autoDetach=false", "--literal-pathspecs"]);
        cmd.args(args);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        cmd
    }
}

/// Finished command: exit status and captured output.
pub(super) struct Output {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Runs to completion within `timeout` (the child is killed on timeout).
pub(super) async fn run(mut cmd: Command, timeout: Duration) -> Result<Output, GitError> {
    let child = cmd.spawn().map_err(|e| GitError::Unavailable(format!("spawn git: {e}")))?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Err(_) => Err(GitError::Timeout),
        Ok(Err(e)) => Err(GitError::Unavailable(format!("git: {e}"))),
        Ok(Ok(out)) => Ok(Output { success: out.status.success(), stdout: out.stdout, stderr: String::from_utf8_lossy(&out.stderr).into_owned() }),
    }
}

/// Runs and maps a non-zero exit to [`classify`].
pub(super) async fn run_ok(cmd: Command, timeout: Duration) -> Result<Vec<u8>, GitError> {
    let out = run(cmd, timeout).await?;
    if out.success { Ok(out.stdout) } else { Err(classify(&out.stderr)) }
}

/// Maps git's stderr (C locale) to a port error. Callers check "object/rev not in the mirror"
/// themselves before running commands, so `bad object` here means a damaged mirror.
pub(super) fn classify(stderr: &str) -> GitError {
    let lower = stderr.to_ascii_lowercase();
    let first = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("git failed").trim().to_owned();
    const AUTH: &[&str] = &[
        "authentication failed",
        "could not read username",
        "could not read password",
        "the requested url returned error: 401",
        "the requested url returned error: 403",
        "invalid username or password",
        "repository not found",
    ];
    const CORRUPT: &[&str] = &[
        "bad object",
        "not a git repository",
        "index-pack failed",
        "loose object",
        "is corrupt",
        "corrupt",
        "packfile",
        "did not receive expected object",
        "missing blob",
        "missing tree",
        "unable to read",
        "invalid object",
        "broken link",
    ];
    if AUTH.iter().any(|p| lower.contains(p)) {
        GitError::Unauthorized
    } else if CORRUPT.iter().any(|p| lower.contains(p)) {
        GitError::Corrupt(first)
    } else {
        GitError::Unavailable(first)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use mage_core::ports::source_host::{AccessToken, InstallationToken};

    use super::*;

    fn spec(url: &str) -> FetchSpec {
        FetchSpec {
            url: url.parse().unwrap(),
            username: "x-access-token".into(),
            token: InstallationToken {
                token: AccessToken::new("ghs_SECRET123"),
                expires_at: time::OffsetDateTime::UNIX_EPOCH,
                permissions: BTreeMap::new(),
                repositories: None,
            },
        }
    }

    #[test]
    fn token_only_in_env_and_url_stripped() {
        let env = GitEnv { bin: "git".into(), home: "/h".into(), path: Some("/usr/bin".into()), allow_file_protocol: false };
        let spec = spec("https://user:pw@github.com:8443/acme/app.git");
        let creds = Credentials::from_spec(&spec);
        let url = remote_url(&spec).unwrap();
        assert_eq!(url, "https://github.com:8443/acme/app.git");
        let cmd = env.command(Some(Path::new("/m/r.git")), ["fetch", "origin", url.as_str()], Some(&creds));
        let std = cmd.as_std();
        let args: Vec<String> = std.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        let basic = base64::engine::general_purpose::STANDARD.encode("x-access-token:ghs_SECRET123");
        assert!(args.iter().all(|a| !a.contains("ghs_SECRET123") && !a.contains(&basic)), "{args:?}");
        assert!(args.windows(2).any(|w| w == ["-c", "protocol.allow=never"]));
        assert!(!args.iter().any(|a| a.contains("protocol.file.allow")));
        let envs: BTreeMap<String, Option<String>> = std
            .get_envs()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned())))
            .collect();
        assert_eq!(envs["GIT_CONFIG_KEY_0"].as_deref(), Some("http.https://github.com:8443/.extraheader"));
        assert_eq!(envs["GIT_CONFIG_VALUE_0"].as_deref(), Some(format!("AUTHORIZATION: basic {basic}").as_str()));
        assert_eq!(envs["HOME"].as_deref(), Some("/h"));
        assert_eq!(envs["GIT_CONFIG_GLOBAL"].as_deref(), Some("/dev/null"));
        // Read commands carry no credentials at all.
        let read = env.command(Some(Path::new("/m/r.git")), ["cat-file", "-e", "x"], None);
        assert!(read.as_std().get_envs().all(|(_, v)| v.is_none_or(|v| !v.to_string_lossy().contains(&basic))));
    }

    #[test]
    fn classify_maps_auth_corrupt_network() {
        assert_eq!(classify("fatal: Authentication failed for 'https://github.com/a/b.git/'"), GitError::Unauthorized);
        assert_eq!(classify("fatal: could not read Username for 'https://github.com': terminal prompts disabled"), GitError::Unauthorized);
        assert_eq!(classify("fatal: unable to access 'x': The requested URL returned error: 403"), GitError::Unauthorized);
        assert!(matches!(classify("error: index-pack failed"), GitError::Corrupt(_)));
        assert!(matches!(classify("fatal: not a git repository: '/m/r.git'"), GitError::Corrupt(_)));
        assert!(matches!(classify("fatal: unable to access 'x': Could not resolve host: github.com"), GitError::Unavailable(_)));
    }
}
