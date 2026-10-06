//! Artifact upload and packaging (runs.md "RequestUpload", "Retry rules": 10 min per artifact).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use mage_core::domain::{ArtifactId, ArtifactKind, RunId};
use mage_core::wire::runner::ArtifactSpec;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::ports::link::{LinkError, ServerLink};

/// Deadline for one artifact: request, PUT and complete together.
pub const ARTIFACT_DEADLINE: Duration = Duration::from_secs(10 * 60);

/// Uploads `path` as an artifact of the run: computes size and sha256, `request_upload`
/// (idempotent on `(run, attempt, kind, name)`), skips the PUT when the grant says the artifact
/// is already complete, PUTs, then `complete_upload`. A PUT refused by the blob host (expired
/// grant) re-requests the grant once. The whole artifact has [`ARTIFACT_DEADLINE`].
pub async fn upload_file(
    link: &dyn ServerLink,
    run: RunId,
    attempt: u16,
    kind: ArtifactKind,
    name: &str,
    content_type: &str,
    path: &Path,
) -> Result<ArtifactId, LinkError> {
    let upload = async {
        if !ArtifactSpec::kind_allowed(kind) {
            return Err(LinkError::Invalid(format!("artifact kind {} cannot be uploaded by a runner", kind.as_str())));
        }
        if !ArtifactSpec::name_valid(name) {
            return Err(LinkError::Invalid(format!("artifact name {name:?} is not valid")));
        }
        let (bytes, sha256) = file_digest(path).await.map_err(|e| LinkError::Invalid(format!("read {}: {e}", path.display())))?;
        if bytes > ArtifactSpec::max_bytes(kind) {
            return Err(LinkError::TooLarge(format!("{name}: {bytes} bytes")));
        }
        let spec = ArtifactSpec { kind, name: name.to_owned(), bytes, sha256, content_type: content_type.to_owned() };
        let mut grant = link.request_upload(run, attempt, &spec).await?;
        if grant.already_complete {
            return Ok(grant.artifact_id);
        }
        match link.upload(&grant, path).await {
            Ok(()) => {}
            Err(LinkError::Forbidden | LinkError::Invalid(_)) => {
                grant = link.request_upload(run, attempt, &spec).await?;
                if grant.already_complete {
                    return Ok(grant.artifact_id);
                }
                link.upload(&grant, path).await?;
            }
            Err(e) => return Err(e),
        }
        link.complete_upload(run, attempt, grant.artifact_id).await?;
        Ok(grant.artifact_id)
    };
    match tokio::time::timeout(ARTIFACT_DEADLINE, upload).await {
        Ok(result) => result,
        Err(_) => Err(LinkError::Unavailable(format!("artifact {name} not uploaded within {} s", ARTIFACT_DEADLINE.as_secs()))),
    }
}

/// Size and sha256 of a file, streamed.
pub async fn file_digest(path: &Path) -> std::io::Result<(u64, [u8; 32])> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((total, hasher.finalize().into()))
}

/// Compresses `src` into `dest` with zstd (level 3).
pub async fn zstd_file(src: PathBuf, dest: PathBuf) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || {
        let mut input = std::fs::File::open(&src)?;
        let output = std::fs::File::create(&dest)?;
        let mut encoder = zstd::Encoder::new(output, 3)?;
        std::io::copy(&mut input, &mut encoder)?;
        encoder.finish()?.sync_all()
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Writes `entries` (`name`, bytes) as a `tar.zst` to `dest`. When the entries exceed `cap`
/// bytes in total, each entry keeps only its newest bytes (an equal share of the cap per entry,
/// shares unused by smaller entries going to the larger ones).
pub async fn tar_zst(entries: Vec<(String, Vec<u8>)>, cap: usize, dest: PathBuf) -> std::io::Result<()> {
    let entries = cap_newest(entries, cap);
    tokio::task::spawn_blocking(move || {
        let output = std::fs::File::create(&dest)?;
        let encoder = zstd::Encoder::new(output, 3)?;
        let mut builder = tar::Builder::new(encoder);
        for (name, bytes) in &entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_cksum();
            builder.append_data(&mut header, name, bytes.as_slice())?;
        }
        let mut encoder = builder.into_inner()?;
        encoder.flush()?;
        encoder.finish()?.sync_all()
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Keeps at most `cap` bytes over all entries, cutting the oldest (leading) bytes of the
/// largest entries first.
fn cap_newest(mut entries: Vec<(String, Vec<u8>)>, cap: usize) -> Vec<(String, Vec<u8>)> {
    let total: usize = entries.iter().map(|(_, b)| b.len()).sum();
    if total <= cap {
        return entries;
    }
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by_key(|&i| entries[i].1.len());
    let mut left = cap;
    let mut remaining = entries.len();
    for i in order {
        let share = left / remaining;
        let bytes = &mut entries[i].1;
        if bytes.len() > share {
            let cut = bytes.len() - share;
            bytes.drain(..cut);
        }
        left -= bytes.len();
        remaining -= 1;
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_keeps_newest_bytes_and_gives_unused_share_to_larger_entries() {
        let entries = vec![("small".to_owned(), b"abc".to_vec()), ("big".to_owned(), (0u8..100).collect::<Vec<u8>>())];
        let capped = cap_newest(entries, 23);
        assert_eq!(capped[0].1, b"abc");
        // 20 bytes left for "big": its newest 20 bytes.
        assert_eq!(capped[1].1, (80u8..100).collect::<Vec<u8>>());
    }

    #[test]
    fn cap_is_a_no_op_under_the_limit() {
        let entries = vec![("a".to_owned(), vec![1, 2, 3])];
        assert_eq!(cap_newest(entries.clone(), 3), entries);
    }
}
