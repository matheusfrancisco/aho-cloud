//! Starting the per-connection aho process.
//!
//! [`CommandTemplate`] turns a workspace into argv; a [`Spawner`] runs it with piped stdio.
//! [`TokioSpawner`] is the real one; tests may substitute their own.

use std::io;
use std::path::PathBuf;
use std::pin::Pin;

use futures::future::BoxFuture;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::process::{Child, ChildStderr, Command};

/// How to launch `ahoc` for a workspace.
///
/// argv = `[--set <kv>]… ++ args ++ (session_args if a session id is known)`. `{dir}` is replaced
/// in `args` and `session_args`, `{session}` in `session_args`. `--set` goes first because it is a
/// global flag and `args` usually ends with the `rpc` subcommand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandTemplate {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub session_args: Vec<String>,
    /// `KEY=VALUE` config overrides, e.g. `seams.world=vm`.
    pub sets: Vec<String>,
}

impl Default for CommandTemplate {
    fn default() -> Self {
        Self {
            program: PathBuf::from("ahoc"),
            args: ["--project", "{dir}", "rpc"].map(String::from).to_vec(),
            session_args: ["--session", "{session}"].map(String::from).to_vec(),
            sets: Vec::new(),
        }
    }
}

impl CommandTemplate {
    /// The command for one connection. `dir` is the project directory and becomes the cwd.
    /// `session` must already be a validated session id (it lands in argv).
    pub fn render(&self, dir: &str, session: Option<&str>, label: &str) -> SpawnSpec {
        let mut args = Vec::with_capacity(self.sets.len() * 2 + self.args.len() + self.session_args.len());
        for kv in &self.sets {
            args.push("--set".to_owned());
            args.push(kv.clone());
        }
        args.extend(self.args.iter().map(|a| a.replace("{dir}", dir)));
        if let Some(session) = session {
            args.extend(self.session_args.iter().map(|a| a.replace("{dir}", dir).replace("{session}", session)));
        }
        SpawnSpec { program: self.program.clone(), args, cwd: PathBuf::from(dir), label: label.to_owned() }
    }
}

/// One concrete command to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Tags the child's stderr in the logs (the workspace id).
    pub label: String,
}

/// A running child's stdio and its handle.
pub struct Spawned {
    pub stdin: Pin<Box<dyn AsyncWrite + Send>>,
    pub stdout: Pin<Box<dyn AsyncRead + Send>>,
    pub process: Box<dyn Process>,
}

/// Waiting on and killing a child.
pub trait Process: Send {
    /// Resolves when the child exits: `Some(code)`, or `None` if a signal ended it. Cancel-safe
    /// and repeatable.
    fn wait(&mut self) -> BoxFuture<'_, io::Result<Option<i32>>>;
    /// Starts a forced kill (`SIGKILL` on Unix); follow with [`Process::wait`].
    fn kill(&mut self) -> io::Result<()>;
}

/// Starts children. Called once per accepted rpc connection, from inside the tokio runtime.
pub trait Spawner: Send + Sync + 'static {
    fn spawn(&self, spec: &SpawnSpec) -> io::Result<Spawned>;
}

/// Env var carrying the service token the control plane presents.
pub const SERVICE_TOKEN_ENV: &str = "AHO_CLOUD_SERVICE_TOKEN";
/// Env var carrying the rpc token signing secret.
pub const SESSION_SECRET_ENV: &str = "AHO_CLOUD_SESSION_SECRET";
/// This service's own secrets. Children run model-driven tools, so these never reach them.
pub const HOST_SECRET_ENV: &[&str] = &[SERVICE_TOKEN_ENV, SESSION_SECRET_ENV];

/// Runs the program with `tokio::process`: stdin/stdout piped for the bridge, stderr forwarded to
/// `tracing` line by line, the environment inherited minus [`HOST_SECRET_ENV`], killed if the
/// handle is dropped.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioSpawner;

impl Spawner for TokioSpawner {
    fn spawn(&self, spec: &SpawnSpec) -> io::Result<Spawned> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .current_dir(&spec.cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        for var in HOST_SECRET_ENV {
            cmd.env_remove(var);
        }
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| io::Error::other("child stdin was not piped"))?;
        let stdout = child.stdout.take().ok_or_else(|| io::Error::other("child stdout was not piped"))?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(forward_stderr(stderr, spec.label.clone()));
        }
        Ok(Spawned { stdin: Box::pin(stdin), stdout: Box::pin(stdout), process: Box::new(TokioProcess(child)) })
    }
}

struct TokioProcess(Child);

impl Process for TokioProcess {
    fn wait(&mut self) -> BoxFuture<'_, io::Result<Option<i32>>> {
        Box::pin(async move { self.0.wait().await.map(|s| s.code()) })
    }

    fn kill(&mut self) -> io::Result<()> {
        self.0.start_kill()
    }
}

async fn forward_stderr(stderr: ChildStderr, label: String) {
    let mut lines = BufReader::new(stderr).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => tracing::info!(target: "ahoc", workspace = %label, "{line}"),
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(target: "ahoc", workspace = %label, "stderr unreadable: {e}");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_come_first_and_the_session_is_appended_only_when_known() {
        let t = CommandTemplate { sets: vec!["seams.world=vm".into()], ..CommandTemplate::default() };
        let fresh = t.render("/data/w/project", None, "w");
        assert_eq!(fresh.args, ["--set", "seams.world=vm", "--project", "/data/w/project", "rpc"]);
        assert_eq!(fresh.cwd, PathBuf::from("/data/w/project"));
        let resumed = t.render("/data/w/project", Some("5e55"), "w");
        assert_eq!(resumed.args, ["--set", "seams.world=vm", "--project", "/data/w/project", "rpc", "--session", "5e55"]);
    }
}
