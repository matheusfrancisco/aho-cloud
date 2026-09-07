//! What runs inside a microVM: the config drive the host writes, the NDJSON control protocol
//! the host speaks over vsock, and the process runner `cloud-guest` (PID 1) uses.

pub mod control;
pub mod init;
pub mod process;

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use cloud_contract::manifest::Health;
use serde::{Deserialize, Serialize};

/// Kernel parameter (passed to init as an environment variable) naming the config drive device.
pub const CONFIG_DRIVE_ENV: &str = "CLOUD_GUEST_CONFIG";
/// Config drive device inside the guest (second virtio-blk drive).
pub const CONFIG_DEVICE: &str = "/dev/vdb";
/// Volume device inside the guest (third virtio-blk drive), when the process has a volume.
pub const VOLUME_DEVICE: &str = "/dev/vdc";
/// vsock port `cloud-guest` serves the control protocol on.
pub const CONTROL_PORT: u32 = 52;
/// Where the host places the `cloud-guest` binary in every rootfs; the kernel's `init=`.
pub const GUEST_INIT_PATH: &str = "/.cloud/guest";
/// `PATH` for processes whose image does not set one.
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// Config drives are padded to a whole number of these.
const DRIVE_BLOCK: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum GuestError {
    #[error("config drive: {0}")]
    Config(String),
    #[error("nothing to run: no command in the manifest and no entrypoint/cmd in the image")]
    NoCommand,
    #[error("user `{0}` not found in the image")]
    User(String),
    #[error("{0}: {1}")]
    Io(String, std::io::Error),
}

/// The image's own run configuration (OCI image config `config` object).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageConfig {
    #[serde(default)]
    pub entrypoint: Vec<String>,
    #[serde(default)]
    pub cmd: Vec<String>,
    /// `KEY=value` entries.
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    /// `user`, `uid`, `user:group` or `uid:gid`.
    #[serde(default)]
    pub user: Option<String>,
}

/// A volume disk and where the process expects it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeMount {
    pub device: String,
    pub mount: String,
}

/// The config drive: everything the guest needs to run one replica of one process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestConfig {
    pub process: String,
    /// The manifest's `command`; `None` runs the image's entrypoint + cmd.
    pub command: Option<Vec<String>>,
    pub image: ImageConfig,
    /// Platform variables and secret values; these override the image's env.
    pub env: BTreeMap<String, String>,
    pub port: Option<u16>,
    pub health: Health,
    pub volume: Option<VolumeMount>,
    /// Resolvers for `/etc/resolv.conf`.
    pub dns: Vec<Ipv4Addr>,
}

impl GuestConfig {
    /// The argv to exec: the manifest command, else the image entrypoint followed by its cmd.
    pub fn argv(&self) -> Result<Vec<String>, GuestError> {
        let argv = match &self.command {
            Some(cmd) => cmd.clone(),
            None => self.image.entrypoint.iter().chain(&self.image.cmd).cloned().collect(),
        };
        if argv.is_empty() {
            return Err(GuestError::NoCommand);
        }
        Ok(argv)
    }

    /// The process environment: the image's env, a default `PATH`, then [`GuestConfig::env`].
    pub fn process_env(&self) -> BTreeMap<String, String> {
        let mut env: BTreeMap<String, String> =
            self.image.env.iter().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
        env.entry("PATH".into()).or_insert_with(|| DEFAULT_PATH.into());
        env.extend(self.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        env
    }

    /// Serializes to the raw drive contents: JSON, NUL-padded to whole blocks.
    pub fn to_drive(&self) -> Result<Vec<u8>, GuestError> {
        let mut bytes = serde_json::to_vec(self).map_err(|e| GuestError::Config(e.to_string()))?;
        let padded = bytes.len().div_ceil(DRIVE_BLOCK).max(1) * DRIVE_BLOCK;
        bytes.resize(padded, 0);
        Ok(bytes)
    }

    /// Parses raw drive contents written by [`GuestConfig::to_drive`].
    pub fn from_drive(bytes: &[u8]) -> Result<Self, GuestError> {
        let end = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        serde_json::from_slice(&bytes[..end]).map_err(|e| GuestError::Config(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(command: Option<Vec<String>>) -> GuestConfig {
        GuestConfig {
            process: "web".into(),
            command,
            image: ImageConfig {
                entrypoint: vec!["/entry".into()],
                cmd: vec!["serve".into(), "--fast".into()],
                env: vec!["PATH=/app/bin".into(), "LANG=C".into(), "PORT=1".into(), "junk".into()],
                working_dir: Some("/app".into()),
                user: None,
            },
            env: BTreeMap::from([("PORT".into(), "8080".into()), ("API_KEY".into(), "s3cret".into())]),
            port: Some(8080),
            health: Health::Http { path: "/healthz".into(), port: None },
            volume: None,
            dns: vec![Ipv4Addr::new(10, 77, 0, 1)],
        }
    }

    #[test]
    fn argv_prefers_the_manifest_command_over_the_image() {
        assert_eq!(config(None).argv().unwrap(), ["/entry", "serve", "--fast"]);
        assert_eq!(config(Some(vec!["node".into(), "x.js".into()])).argv().unwrap(), ["node", "x.js"]);
        let mut empty = config(None);
        empty.image = ImageConfig::default();
        assert!(matches!(empty.argv(), Err(GuestError::NoCommand)));
        assert!(matches!(config(Some(vec![])).argv(), Err(GuestError::NoCommand)));
    }

    #[test]
    fn platform_env_overrides_image_env_and_path_defaults() {
        let env = config(None).process_env();
        assert_eq!(env["PORT"], "8080");
        assert_eq!(env["PATH"], "/app/bin");
        assert_eq!(env["LANG"], "C");
        assert_eq!(env["API_KEY"], "s3cret");
        assert!(!env.contains_key("junk"));
        let mut bare = config(None);
        bare.image.env.clear();
        assert_eq!(bare.process_env()["PATH"], DEFAULT_PATH);
    }

    #[test]
    fn drive_round_trips_through_block_padding() {
        let c = config(None);
        let drive = c.to_drive().unwrap();
        assert_eq!(drive.len() % DRIVE_BLOCK, 0);
        assert_eq!(GuestConfig::from_drive(&drive).unwrap(), c);
        assert!(GuestConfig::from_drive(&[0; 4096]).is_err());
    }
}
