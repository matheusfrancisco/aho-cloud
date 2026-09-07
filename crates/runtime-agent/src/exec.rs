//! Running the host programs the agent drives (skopeo, umoci, mkfs.ext4, ip, nft).

use std::ffi::OsStr;
use std::path::Path;
use std::process::Stdio;

/// Most of a failing program's stderr kept in the error.
const STDERR_TAIL: usize = 2048;

/// A host program failed to start or exited non-zero.
#[derive(Debug, thiserror::Error)]
#[error("{program}: {detail}")]
pub struct ExecError {
    pub program: String,
    pub detail: String,
}

/// Runs `program args…` to completion; its stdout on success.
pub async fn run<I, S>(program: &Path, args: I) -> Result<String, ExecError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let name = program.display().to_string();
    let out = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| ExecError { program: name.clone(), detail: e.to_string() })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let start = stderr.len().saturating_sub(STDERR_TAIL);
        let start = (start..stderr.len()).find(|&i| stderr.is_char_boundary(i)).unwrap_or(stderr.len());
        return Err(ExecError { program: name, detail: format!("{}: {}", out.status, stderr[start..].trim()) });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
