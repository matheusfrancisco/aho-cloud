//! OCI image → ext4 root filesystem. Pulled once per digest (`skopeo copy` into an OCI
//! layout, `umoci unpack` into a bundle), then every replica gets its own writable ext4 image
//! built with `mkfs.ext4 -d`. The `cloud-guest` init is placed at [`GUEST_INIT_PATH`].

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::Deserialize;

use crate::exec::{ExecError, run};
use crate::guest::{GUEST_INIT_PATH, ImageConfig};

/// Tag the layout is written under.
const LAYOUT_TAG: &str = "img";
/// Written last; its presence means the cache entry is complete.
const READY: &str = "image-config.json";

#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("invalid image digest `{0}` (want sha256:<64 hex>)")]
    Digest(String),
    #[error(transparent)]
    Exec(#[from] ExecError),
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("OCI layout: {0}")]
    Oci(String),
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> ImageError + '_ {
    move |source| ImageError::Io { path: path.to_owned(), source }
}

/// The external programs, by path (bare names are looked up on `PATH`).
#[derive(Clone, Debug)]
pub struct ImagePrograms {
    pub skopeo: PathBuf,
    pub umoci: PathBuf,
    pub mkfs_ext4: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ImageStoreConfig {
    /// Cache directory: one subdirectory per digest.
    pub dir: PathBuf,
    pub programs: ImagePrograms,
    /// Registry repository per project; `{project}` is substituted, e.g.
    /// `registry.internal:5000/aho-cloud/{project}`.
    pub repo_template: String,
    /// The `cloud-guest` binary copied into every rootfs.
    pub guest_binary: PathBuf,
}

/// An unpacked image ready to be turned into disks.
#[derive(Clone, Debug)]
pub struct PreparedImage {
    pub rootfs: PathBuf,
    pub config: ImageConfig,
}

/// `sha256:<64 lowercase hex>` → the hex part.
pub fn digest_hex(digest: &str) -> Result<&str, ImageError> {
    match digest.strip_prefix("sha256:") {
        Some(hex) if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) => Ok(hex),
        _ => Err(ImageError::Digest(digest.to_owned())),
    }
}

/// `docker://<repo>@<digest>` for skopeo.
pub fn image_ref(repo_template: &str, project: &str, digest: &str) -> String {
    format!("docker://{}@{digest}", repo_template.replace("{project}", project))
}

pub struct ImageStore {
    cfg: ImageStoreConfig,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl ImageStore {
    pub fn new(cfg: ImageStoreConfig) -> Self {
        ImageStore { cfg, locks: Mutex::new(HashMap::new()) }
    }

    /// Pulls and unpacks `digest` unless cached; concurrent callers for one digest wait for
    /// the first.
    pub async fn prepare(&self, project: &str, digest: &str) -> Result<PreparedImage, ImageError> {
        let hex = digest_hex(digest)?.to_owned();
        let lock = self.locks.lock().entry(hex.clone()).or_default().clone();
        let _guard = lock.lock().await;
        let dir = self.cfg.dir.join(&hex);
        let bundle = dir.join("bundle");
        let rootfs = bundle.join("rootfs");
        let ready = dir.join(READY);
        let config = match tokio::fs::read(&ready).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| ImageError::Oci(format!("{READY}: {e}")))?,
            Err(_) => {
                match tokio::fs::remove_dir_all(&dir).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(io_err(&dir)(e)),
                }
                tokio::fs::create_dir_all(&dir).await.map_err(io_err(&dir))?;
                let layout = dir.join("layout");
                let source = image_ref(&self.cfg.repo_template, project, digest);
                let dest = format!("oci:{}:{LAYOUT_TAG}", layout.display());
                run(&self.cfg.programs.skopeo, ["copy", source.as_str(), dest.as_str()]).await?;
                let image = format!("{}:{LAYOUT_TAG}", layout.display());
                run(&self.cfg.programs.umoci, [OsStr::new("unpack"), OsStr::new("--image"), OsStr::new(&image), bundle.as_os_str()])
                    .await?;
                let config = read_image_config(&layout, LAYOUT_TAG)?;
                tokio::fs::remove_dir_all(&layout).await.map_err(io_err(&layout))?;
                let json = serde_json::to_vec(&config).map_err(|e| ImageError::Oci(e.to_string()))?;
                tokio::fs::write(&ready, json).await.map_err(io_err(&ready))?;
                config
            }
        };
        install_guest(&self.cfg.guest_binary, &rootfs).await?;
        Ok(PreparedImage { rootfs, config })
    }

    /// Builds a fresh ext4 image at `out` holding `image`'s rootfs.
    pub async fn rootfs_disk(&self, image: &PreparedImage, out: &Path, size_mib: u32) -> Result<(), ImageError> {
        let size = format!("{size_mib}M");
        let args = [OsStr::new("-q"), OsStr::new("-F"), OsStr::new("-d"), image.rootfs.as_os_str(), out.as_os_str(), OsStr::new(&size)];
        run(&self.cfg.programs.mkfs_ext4, args).await?;
        Ok(())
    }

    /// Builds an empty ext4 image at `out` (a new volume).
    pub async fn empty_disk(&self, out: &Path, size_mib: u32) -> Result<(), ImageError> {
        let size = format!("{size_mib}M");
        run(&self.cfg.programs.mkfs_ext4, [OsStr::new("-q"), OsStr::new("-F"), out.as_os_str(), OsStr::new(&size)]).await?;
        Ok(())
    }
}

/// Copies the init into `<rootfs>/.cloud/guest` unless an identical copy is there. Symlinks
/// the image placed on that path are removed first, so the copy can never land on the host.
async fn install_guest(binary: &Path, rootfs: &Path) -> Result<(), ImageError> {
    let rel = GUEST_INIT_PATH.trim_start_matches('/');
    let dest = rootfs.join(rel);
    let Some(dir) = dest.parent() else {
        return Err(ImageError::Oci(format!("bad init path {GUEST_INIT_PATH}")));
    };
    match tokio::fs::symlink_metadata(dir).await {
        Ok(m) if m.is_dir() => {}
        Ok(_) => tokio::fs::remove_file(dir).await.map_err(io_err(dir))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(dir)(e)),
    }
    tokio::fs::create_dir_all(dir).await.map_err(io_err(dir))?;
    let wanted = tokio::fs::read(binary).await.map_err(io_err(binary))?;
    match tokio::fs::symlink_metadata(&dest).await {
        Ok(m) if m.is_file() => {
            if tokio::fs::read(&dest).await.is_ok_and(|have| blake3::hash(&have) == blake3::hash(&wanted)) {
                return Ok(());
            }
            tokio::fs::remove_file(&dest).await.map_err(io_err(&dest))?;
        }
        Ok(m) if m.is_dir() => tokio::fs::remove_dir_all(&dest).await.map_err(io_err(&dest))?,
        Ok(_) => tokio::fs::remove_file(&dest).await.map_err(io_err(&dest))?,
        Err(_) => {}
    }
    tokio::fs::write(&dest, &wanted).await.map_err(io_err(&dest))?;
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).await.map_err(io_err(&dest))
}

#[derive(Deserialize)]
struct Descriptor {
    digest: String,
    #[serde(default)]
    annotations: HashMap<String, String>,
}

#[derive(Deserialize)]
struct Index {
    manifests: Vec<Descriptor>,
}

#[derive(Deserialize)]
struct Manifest {
    config: Descriptor,
}

#[derive(Deserialize)]
struct ImageBlob {
    #[serde(default)]
    config: Option<RunConfig>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RunConfig {
    #[serde(default)]
    entrypoint: Option<Vec<String>>,
    #[serde(default)]
    cmd: Option<Vec<String>>,
    #[serde(default)]
    env: Option<Vec<String>>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    user: Option<String>,
}

fn blob(layout: &Path, digest: &str) -> Result<Vec<u8>, ImageError> {
    let (alg, hex) = digest.split_once(':').ok_or_else(|| ImageError::Oci(format!("bad digest {digest}")))?;
    let ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric());
    if !ok(alg) || !ok(hex) {
        return Err(ImageError::Oci(format!("bad digest {digest}")));
    }
    let path = layout.join("blobs").join(alg).join(hex);
    std::fs::read(&path).map_err(io_err(&path))
}

fn parse<'a, T: Deserialize<'a>>(what: &str, bytes: &'a [u8]) -> Result<T, ImageError> {
    serde_json::from_slice(bytes).map_err(|e| ImageError::Oci(format!("{what}: {e}")))
}

/// The run configuration (entrypoint, cmd, env, working dir, user) of image `tag` in an OCI
/// layout directory.
pub fn read_image_config(layout: &Path, tag: &str) -> Result<ImageConfig, ImageError> {
    let index_path = layout.join("index.json");
    let index_bytes = std::fs::read(&index_path).map_err(io_err(&index_path))?;
    let index: Index = parse("index.json", &index_bytes)?;
    let by_tag = index.manifests.iter().find(|d| d.annotations.get("org.opencontainers.image.ref.name").is_some_and(|t| t == tag));
    let descriptor = match (by_tag, index.manifests.as_slice()) {
        (Some(d), _) => d,
        (None, [only]) => only,
        _ => return Err(ImageError::Oci(format!("no manifest tagged `{tag}`"))),
    };
    let manifest_bytes = blob(layout, &descriptor.digest)?;
    let manifest: Manifest = parse("manifest", &manifest_bytes)?;
    let config_bytes = blob(layout, &manifest.config.digest)?;
    let image: ImageBlob = parse("image config", &config_bytes)?;
    let Some(run) = image.config else {
        return Ok(ImageConfig::default());
    };
    Ok(ImageConfig {
        entrypoint: run.entrypoint.unwrap_or_default(),
        cmd: run.cmd.unwrap_or_default(),
        env: run.env.unwrap_or_default(),
        working_dir: run.working_dir.filter(|d| !d.is_empty()),
        user: run.user.filter(|u| !u.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a minimal OCI layout whose config is `config_json`.
    fn write_layout(layout: &Path, config_json: &str) {
        let blobs = layout.join("blobs/sha256");
        std::fs::create_dir_all(&blobs).unwrap();
        let config_hex = "c".repeat(64);
        let manifest_hex = "a".repeat(64);
        std::fs::write(blobs.join(&config_hex), config_json).unwrap();
        std::fs::write(
            blobs.join(&manifest_hex),
            format!(r#"{{"schemaVersion":2,"config":{{"digest":"sha256:{config_hex}","size":1}},"layers":[]}}"#),
        )
        .unwrap();
        std::fs::write(
            layout.join("index.json"),
            format!(
                r#"{{"schemaVersion":2,"manifests":[{{"digest":"sha256:{manifest_hex}","size":1,"annotations":{{"org.opencontainers.image.ref.name":"img"}}}}]}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn digests_must_be_sha256_hex() {
        let good = format!("sha256:{}", "0123456789abcdef".repeat(4));
        assert!(digest_hex(&good).is_ok());
        for bad in [
            "sha256:abc".to_owned(),
            format!("sha512:{}", "a".repeat(64)),
            format!("sha256:{}", "A".repeat(64)),
            format!("sha256:{}/../x", "a".repeat(60)),
        ] {
            assert!(digest_hex(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn image_ref_substitutes_the_project() {
        assert_eq!(image_ref("reg:5000/aho-cloud/{project}", "shop", "sha256:ab"), "docker://reg:5000/aho-cloud/shop@sha256:ab");
    }

    #[test]
    fn run_config_is_read_from_the_layout() {
        let dir = tempfile::tempdir().unwrap();
        write_layout(
            dir.path(),
            r#"{"architecture":"amd64","config":{"Entrypoint":["/bin/app"],"Cmd":["--serve"],"Env":["A=1"],"WorkingDir":"/srv","User":"app"}}"#,
        );
        let c = read_image_config(dir.path(), "img").unwrap();
        assert_eq!(c.entrypoint, ["/bin/app"]);
        assert_eq!(c.cmd, ["--serve"]);
        assert_eq!(c.env, ["A=1"]);
        assert_eq!(c.working_dir.as_deref(), Some("/srv"));
        assert_eq!(c.user.as_deref(), Some("app"));

        let bare = tempfile::tempdir().unwrap();
        write_layout(bare.path(), r#"{"config":{"Entrypoint":null,"WorkingDir":"","User":""}}"#);
        assert_eq!(read_image_config(bare.path(), "other").unwrap(), ImageConfig::default());
    }

    #[test]
    fn blob_digests_cannot_traverse() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(blob(dir.path(), "sha256:../../etc/passwd"), Err(ImageError::Oci(_))));
    }

    #[tokio::test]
    async fn guest_install_replaces_symlinks_instead_of_following_them() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let rootfs = dir.path().join("rootfs");
        std::fs::create_dir(&rootfs).unwrap();
        std::os::unix::fs::symlink(&outside, rootfs.join(".cloud")).unwrap();
        let binary = dir.path().join("cloud-guest");
        std::fs::write(&binary, b"#!guest").unwrap();

        install_guest(&binary, &rootfs).await.unwrap();
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none(), "wrote through the symlink");
        let installed = rootfs.join(".cloud/guest");
        assert_eq!(std::fs::read(&installed).unwrap(), b"#!guest");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&installed).unwrap().permissions().mode() & 0o777, 0o755);

        std::fs::remove_file(&installed).unwrap();
        std::os::unix::fs::symlink(outside.join("target"), &installed).unwrap();
        install_guest(&binary, &rootfs).await.unwrap();
        assert!(!outside.join("target").exists());
        assert_eq!(std::fs::read(&installed).unwrap(), b"#!guest");
    }
}
