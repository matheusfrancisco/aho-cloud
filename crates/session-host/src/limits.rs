//! Connection limits and the admission gate.
//!
//! A workspace's newest connection wins: when a workspace is at its limit, [`Gate::acquire`]
//! asks the connections holding it to leave (they close with [`crate::bridge::CLOSE_REPLACED`])
//! and waits for their slots. That keeps aho's one-writer rule while letting a reloaded page or
//! a second tab take over instead of failing. The host-wide limit never replaces anyone.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::{Notify, watch};

/// Per-connection, per-host and per-request bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Open rpc connections (= running children) across all workspaces.
    pub max_connections: usize,
    /// Open rpc connections per workspace. aho's session log has one writer, so the default is 1.
    pub max_per_workspace: usize,
    /// A connection with no message in either direction for this long is closed.
    pub idle_timeout: Duration,
    /// After the child's stdin is closed, how long it gets to exit before it is killed.
    pub kill_grace: Duration,
    /// Largest client text message, in bytes.
    pub max_frame_bytes: usize,
    /// Largest child stdout line, in bytes.
    pub max_line_bytes: usize,
    /// Largest workspace archive a package may carry (compressed), in bytes; above it the
    /// package route answers 413.
    pub max_archive_bytes: u64,
    /// Most text a file read returns, in bytes; longer files come back truncated.
    pub max_file_bytes: u64,
    /// Most entries a file tree lists; bigger trees come back truncated.
    pub max_tree_entries: usize,
}

impl Limits {
    pub const DEFAULT: Limits = Limits {
        max_connections: 256,
        max_per_workspace: 1,
        idle_timeout: Duration::from_secs(15 * 60),
        kill_grace: Duration::from_secs(10),
        max_frame_bytes: 1 << 20,
        max_line_bytes: 16 << 20,
        max_archive_bytes: 100 << 20,
        max_file_bytes: 1 << 20,
        max_tree_entries: 5000,
    };
}

impl Default for Limits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Which limit refused a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Denied {
    #[error("the session host is at its connection limit; try again later")]
    Host,
    /// The connections holding the workspace did not leave within the wait.
    #[error("this workspace already has its maximum number of open connections")]
    Workspace,
}

/// Counts open connections; a [`Permit`] is one slot, released on drop.
#[derive(Debug)]
pub(crate) struct Gate {
    max_total: usize,
    max_per_workspace: usize,
    counts: Mutex<Counts>,
    /// Woken whenever a permit is released.
    freed: Notify,
}

#[derive(Debug, Default)]
struct Counts {
    total: usize,
    per_workspace: HashMap<String, Slot>,
}

#[derive(Debug)]
struct Slot {
    open: usize,
    /// Bumped to ask every current holder to leave.
    replace: watch::Sender<u64>,
}

impl Gate {
    pub(crate) fn new(limits: &Limits) -> Arc<Self> {
        Arc::new(Self {
            max_total: limits.max_connections,
            max_per_workspace: limits.max_per_workspace,
            counts: Mutex::new(Counts::default()),
            freed: Notify::new(),
        })
    }

    /// A slot now, or which limit is full.
    pub(crate) fn try_acquire(self: &Arc<Self>, workspace: &str) -> Result<Permit, Denied> {
        let mut c = self.lock();
        if c.total >= self.max_total {
            return Err(Denied::Host);
        }
        let slot = c.per_workspace.entry(workspace.to_owned()).or_insert_with(|| Slot { open: 0, replace: watch::Sender::new(0) });
        if slot.open >= self.max_per_workspace {
            return Err(Denied::Workspace);
        }
        slot.open += 1;
        let replaced = slot.replace.subscribe();
        c.total += 1;
        Ok(Permit { gate: Arc::clone(self), workspace: workspace.to_owned(), replaced })
    }

    /// A slot, replacing the workspace's current connections if it is full: they are asked to
    /// leave and this waits up to `wait` for one of them to go. The host limit is not waited on.
    pub(crate) async fn acquire(self: &Arc<Self>, workspace: &str, wait: Duration) -> Result<Permit, Denied> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            // Registered before trying, so a release between the try and the wait is not missed.
            let freed = self.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            match self.try_acquire(workspace) {
                Err(Denied::Workspace) => {
                    self.ask_to_leave(workspace);
                    if tokio::time::timeout_at(deadline, freed).await.is_err() {
                        return Err(Denied::Workspace);
                    }
                }
                other => return other,
            }
        }
    }

    fn ask_to_leave(&self, workspace: &str) {
        if let Some(slot) = self.lock().per_workspace.get(workspace) {
            slot.replace.send_modify(|generation| *generation += 1);
        }
    }

    fn lock(&self) -> MutexGuard<'_, Counts> {
        self.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One admitted connection.
#[derive(Debug)]
pub(crate) struct Permit {
    gate: Arc<Gate>,
    workspace: String,
    replaced: watch::Receiver<u64>,
}

impl Permit {
    /// Resolves once a newer connection to the same workspace asked this one to leave.
    pub(crate) async fn replaced(&mut self) {
        if self.replaced.changed().await.is_err() {
            // The slot outlives every permit counted in it; a closed channel cannot mean "leave".
            std::future::pending::<()>().await;
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        {
            let mut c = self.gate.lock();
            c.total = c.total.saturating_sub(1);
            if let Some(slot) = c.per_workspace.get_mut(&self.workspace) {
                slot.open = slot.open.saturating_sub(1);
                if slot.open == 0 {
                    c.per_workspace.remove(&self.workspace);
                }
            }
        }
        self.gate.freed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(total: usize, per: usize) -> Arc<Gate> {
        Gate::new(&Limits { max_connections: total, max_per_workspace: per, ..Limits::DEFAULT })
    }

    #[test]
    fn per_workspace_limit_is_independent_per_workspace() {
        let g = gate(10, 1);
        let a = g.try_acquire("a").unwrap();
        assert_eq!(g.try_acquire("a").err(), Some(Denied::Workspace));
        let _b = g.try_acquire("b").unwrap();
        drop(a);
        assert!(g.try_acquire("a").is_ok());
    }

    #[test]
    fn host_limit_counts_every_workspace_and_frees_on_drop() {
        let g = gate(2, 5);
        let a = g.try_acquire("a").unwrap();
        let _b = g.try_acquire("b").unwrap();
        assert_eq!(g.try_acquire("c").err(), Some(Denied::Host));
        assert_eq!(g.try_acquire("a").err(), Some(Denied::Host));
        drop(a);
        let _c = g.try_acquire("c").unwrap();
        assert!(!g.lock().per_workspace.contains_key("a"), "empty entries are forgotten");
    }

    #[tokio::test]
    async fn a_newer_connection_asks_the_holder_to_leave_and_gets_its_slot() {
        let g = gate(10, 1);
        let mut old = g.try_acquire("a").unwrap();
        let newer = tokio::spawn({
            let g = Arc::clone(&g);
            async move { g.acquire("a", Duration::from_secs(5)).await }
        });
        tokio::time::timeout(Duration::from_secs(5), old.replaced()).await.expect("the holder is asked to leave");
        drop(old);
        let mut permit = newer.await.unwrap().unwrap();
        // The new holder was not asked to leave by its own takeover.
        assert!(tokio::time::timeout(Duration::from_millis(50), permit.replaced()).await.is_err());
    }

    #[tokio::test]
    async fn a_holder_that_stays_keeps_the_slot_and_the_newcomer_is_refused() {
        let g = gate(10, 1);
        let _stays = g.try_acquire("a").unwrap();
        assert_eq!(g.acquire("a", Duration::from_millis(100)).await.err(), Some(Denied::Workspace));
    }

    #[tokio::test]
    async fn the_host_limit_is_never_waited_on() {
        let g = gate(1, 1);
        let _a = g.try_acquire("a").unwrap();
        let started = std::time::Instant::now();
        assert_eq!(g.acquire("b", Duration::from_secs(5)).await.err(), Some(Denied::Host));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
