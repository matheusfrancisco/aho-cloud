//! Sandboxes: a VM with aho installed and a persistent disk at `/workspace`, owned by an org and
//! usable by every member. The store keeps the record and the sealed environment; the runtime
//! agent runs the VM from a [`SandboxSpec`]; the session host bridges terminals and chats to it;
//! a public port is served by the edge (see [`crate::sites`]).
//!
//! Changes to one sandbox happen one at a time ([`SlugLocks`]); reads overlay the runtime's live
//! status without writing it back.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use cloud_api::internal::SandboxSpec;
use cloud_api::{
    DEFAULT_SANDBOX_EGRESS, SANDBOX_AUTO_STOP_DEFAULT_HOURS, SANDBOX_AUTO_STOP_MAX_HOURS, Sandbox, SandboxSettings, SandboxState,
};
use cloud_contract::validate::is_hostname;
use tokio::sync::OwnedMutexGuard;

use crate::auth::user_view;
use crate::config::SandboxImage;
use crate::error::AppError;
use crate::state::AppState;
use crate::store::SandboxRec;

/// Ports below this are privileged inside the VM: the sandbox user (uid 1000) cannot listen on
/// them, so none of them can be the public port.
pub const FIRST_PUBLIC_PORT: u16 = 1024;
/// Most egress entries one sandbox may list.
pub const MAX_EGRESS: usize = 256;

/// The settings of a sandbox created without any.
pub fn default_settings() -> SandboxSettings {
    SandboxSettings {
        egress: DEFAULT_SANDBOX_EGRESS.iter().map(|h| (*h).to_owned()).collect(),
        port: None,
        auto_stop_hours: SANDBOX_AUTO_STOP_DEFAULT_HOURS,
    }
}

/// `settings` checked, with its egress list trimmed, lowercased and deduplicated: hosts or
/// `*.` wildcards, a public port of [`FIRST_PUBLIC_PORT`]`..=65535`, auto-stop after
/// `1..=`[`SANDBOX_AUTO_STOP_MAX_HOURS`] hours.
pub fn validated(mut settings: SandboxSettings) -> Result<SandboxSettings, AppError> {
    let mut egress: Vec<String> = Vec::with_capacity(settings.egress.len());
    for raw in &settings.egress {
        let host = raw.trim().to_ascii_lowercase();
        if !is_hostname(&host, true) {
            return Err(AppError::invalid(format!("egress: `{raw}` is not a host (a hostname, optionally with a leading `*.`)")));
        }
        if !egress.contains(&host) {
            egress.push(host);
        }
    }
    if egress.len() > MAX_EGRESS {
        return Err(AppError::invalid(format!("egress: at most {MAX_EGRESS} hosts")));
    }
    settings.egress = egress;
    if let Some(port) = settings.port
        && port < FIRST_PUBLIC_PORT
    {
        return Err(AppError::invalid(format!("port {port} is reserved; a public port is {FIRST_PUBLIC_PORT}–65535")));
    }
    if !(1..=SANDBOX_AUTO_STOP_MAX_HOURS).contains(&settings.auto_stop_hours) {
        return Err(AppError::invalid(format!("auto_stop_hours: 1–{SANDBOX_AUTO_STOP_MAX_HOURS}")));
    }
    Ok(settings)
}

/// The image sandboxes boot, unless this deployment runs without one (503 `sandboxes_disabled`).
pub fn image(state: &AppState) -> Result<&SandboxImage, AppError> {
    state.cfg.sandbox_image.as_ref().ok_or_else(|| AppError::Unavailable {
        code: "sandboxes_disabled",
        message: "sandboxes are turned off on this deployment (the control plane runs without --sandbox-image)".into(),
    })
}

/// What the runtime runs `rec` with: its image, settings, size and decrypted environment.
async fn spec(state: &AppState, rec: &SandboxRec) -> Result<SandboxSpec, AppError> {
    let image = SandboxImage::parse(&rec.image)
        .ok_or_else(|| AppError::internal(format!("sandbox {} records an unusable image `{}`", rec.slug, rec.image)))?;
    let mut env = BTreeMap::new();
    for var in state.store.sandbox_env(&rec.slug).await? {
        env.insert(var.name.clone(), state.secrets.open_env(&rec.slug, &var.name, &var.ciphertext)?);
    }
    Ok(SandboxSpec {
        slug: rec.slug.clone(),
        image_repo: image.repo,
        image_digest: image.digest,
        env,
        egress: rec.settings.egress.clone(),
        port: rec.settings.port,
        cpus: rec.cpus,
        mem_mib: rec.mem_mib,
        disk_mib: rec.disk_mib,
    })
}

/// Puts `rec`'s spec to the runtime (booting the VM, or replacing it when the spec changed; the
/// disk is kept), records the state it answers and restarts the auto-stop clock. A put that
/// fails leaves the sandbox `failed` with the reason, and the error is returned.
pub async fn boot(state: &AppState, rec: &mut SandboxRec) -> Result<(), AppError> {
    let spec = spec(state, rec).await?;
    let answer = state.services.runtime.put_sandbox(spec).await;
    match &answer {
        Ok(status) => {
            rec.state = status.state;
            rec.error = status.error.clone();
        }
        Err(e) => {
            rec.state = SandboxState::Failed;
            rec.error = Some(e.to_string());
        }
    }
    rec.stops_at = Some(state.now() + u64::from(rec.settings.auto_stop_hours) * 3600);
    state.store.update_sandbox(rec).await?;
    answer.map(drop).map_err(AppError::from)
}

/// Stops `rec`'s VM (the disk is kept) and its auto-stop clock.
pub async fn stop(state: &AppState, rec: &mut SandboxRec) -> Result<(), AppError> {
    let status = state.services.runtime.stop_sandbox(&rec.slug).await?;
    // No VM at all is stopped too.
    rec.state = status.as_ref().map_or(SandboxState::Stopped, |s| s.state);
    rec.error = status.and_then(|s| s.error);
    rec.stops_at = None;
    state.store.update_sandbox(rec).await?;
    Ok(())
}

/// Overlays the runtime's live state and error on `rec` (not persisted). A recorded stop needs
/// no asking; when the runtime cannot be asked, the recorded state stands.
pub async fn live(state: &AppState, rec: &mut SandboxRec) {
    if rec.state == SandboxState::Stopped {
        return;
    }
    match state.services.runtime.sandbox_status(&rec.slug).await {
        Ok(Some(status)) => {
            rec.state = status.state;
            rec.error = status.error;
        }
        Ok(None) => {
            rec.state = SandboxState::Stopped;
            rec.error = None;
        }
        Err(error) => tracing::warn!(sandbox = %rec.slug, %error, "sandbox status unavailable; showing the recorded state"),
    }
    if rec.state == SandboxState::Stopped {
        rec.stops_at = None;
    }
}

/// The API view of `rec`.
pub async fn view(state: &AppState, rec: &SandboxRec) -> Result<Sandbox, AppError> {
    let creator = state
        .store
        .user(&rec.created_by)
        .await?
        .ok_or_else(|| AppError::internal(format!("sandbox {} names unknown user {}", rec.slug, rec.created_by)))?;
    Ok(Sandbox {
        slug: rec.slug.clone(),
        org: rec.org.clone(),
        state: rec.state,
        created_by: user_view(&creator),
        created_at: rec.created_at,
        image: rec.image.clone(),
        settings: rec.settings.clone(),
        url: rec.settings.port.map(|_| state.cfg.preview_url(&rec.slug)),
        stops_at: rec.stops_at,
        cpus: rec.cpus,
        mem_mib: rec.mem_mib,
        disk_mib: rec.disk_mib,
        error: rec.error.clone(),
    })
}

/// Stops every sandbox whose auto-stop time has passed (the bin runs this periodically). Returns
/// how many were stopped; one that fails to stop is retried on the next round.
pub async fn auto_stop(state: &AppState) -> Result<usize, AppError> {
    let now = state.now();
    let due = |s: &SandboxRec| s.stops_at.is_some_and(|t| t <= now);
    let mut stopped = 0;
    for candidate in state.store.sandboxes().await?.into_iter().filter(due) {
        let _turn = state.sandbox_locks.lock(&candidate.slug).await;
        // Started again, stopped or deleted meanwhile?
        let Some(mut rec) = state.store.sandbox(&candidate.slug).await?.filter(due) else {
            continue;
        };
        match stop(state, &mut rec).await {
            Ok(()) => {
                stopped += 1;
                tracing::info!(sandbox = %rec.slug, org = %rec.org, "sandbox auto-stopped");
            }
            Err(error) => tracing::warn!(sandbox = %rec.slug, %error, "sandbox auto-stop failed; retrying next round"),
        }
    }
    Ok(stopped)
}

/// One async lock per sandbox slug, dropped from the table once nobody holds or awaits it.
#[derive(Default)]
pub struct SlugLocks(parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>);

/// Held while one change to a sandbox runs.
pub struct SlugGuard<'a> {
    locks: &'a SlugLocks,
    slug: String,
    guard: Option<OwnedMutexGuard<()>>,
}

impl SlugLocks {
    /// Waits for the turn on `slug`.
    pub async fn lock(&self, slug: &str) -> SlugGuard<'_> {
        let lock = self.0.lock().entry(slug.to_owned()).or_default().clone();
        SlugGuard { locks: self, slug: slug.to_owned(), guard: Some(lock.lock_owned().await) }
    }
}

impl Drop for SlugGuard<'_> {
    fn drop(&mut self) {
        drop(self.guard.take());
        let mut table = self.locks.0.lock();
        // Every holder and waiter keeps a clone; only the table's is left when nobody does.
        if table.get(&self.slug).is_some_and(|lock| Arc::strong_count(lock) == 1) {
            table.remove(&self.slug);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(egress: &[&str], port: Option<u16>, hours: u16) -> SandboxSettings {
        SandboxSettings { egress: egress.iter().map(|h| (*h).to_owned()).collect(), port, auto_stop_hours: hours }
    }

    #[test]
    fn defaults_pass_their_own_validation() {
        assert_eq!(validated(default_settings()).unwrap(), default_settings());
    }

    #[test]
    fn settings_bounds() {
        let ok = validated(settings(&[" API.Example.com ", "*.example.org", "api.example.com"], Some(1024), 1)).unwrap();
        assert_eq!(ok.egress, ["api.example.com", "*.example.org"], "trimmed, lowercased, deduplicated");
        assert!(validated(settings(&[], Some(65535), SANDBOX_AUTO_STOP_MAX_HOURS)).is_ok());
        for bad in [
            settings(&["not a host"], None, 8),
            settings(&["https://example.com"], None, 8),
            settings(&["example.com:443"], None, 8),
            settings(&["*.*.example.com"], None, 8),
            settings(&["localhost"], None, 8),
            settings(&[], Some(0), 8),
            settings(&[], Some(80), 8),
            settings(&[], Some(1023), 8),
            settings(&[], None, 0),
            settings(&[], None, SANDBOX_AUTO_STOP_MAX_HOURS + 1),
        ] {
            assert!(matches!(validated(bad.clone()), Err(AppError::Invalid(_))), "{bad:?}");
        }
        let many: Vec<String> = (0..=MAX_EGRESS).map(|i| format!("h{i}.example.com")).collect();
        assert!(validated(SandboxSettings { egress: many, port: None, auto_stop_hours: 8 }).is_err());
    }

    #[tokio::test]
    async fn slug_locks_serialize_one_slug_and_forget_idle_ones() {
        let locks = Arc::new(SlugLocks::default());
        let first = locks.lock("dev").await;
        let other = tokio::time::timeout(std::time::Duration::from_millis(50), locks.lock("web")).await;
        assert!(other.is_ok(), "another slug is not blocked");
        drop(other);
        let waiter = {
            let locks = locks.clone();
            tokio::spawn(async move {
                let _g = locks.lock("dev").await;
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "the same slug waits");
        drop(first);
        waiter.await.unwrap();
        assert!(locks.0.lock().is_empty(), "no entry outlives its last holder");
    }
}
