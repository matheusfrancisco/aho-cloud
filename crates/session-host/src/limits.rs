//! Connection limits and the admission gate.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// Per-connection and per-host bounds.
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
}

impl Limits {
    pub const DEFAULT: Limits = Limits {
        max_connections: 256,
        max_per_workspace: 1,
        idle_timeout: Duration::from_secs(15 * 60),
        kill_grace: Duration::from_secs(10),
        max_frame_bytes: 1 << 20,
        max_line_bytes: 16 << 20,
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
    #[error("this workspace already has its maximum number of open connections")]
    Workspace,
}

/// Counts open connections; a [`Permit`] is one slot, released on drop.
#[derive(Debug)]
pub(crate) struct Gate {
    max_total: usize,
    max_per_workspace: usize,
    counts: Mutex<Counts>,
}

#[derive(Debug, Default)]
struct Counts {
    total: usize,
    per_workspace: HashMap<String, usize>,
}

impl Gate {
    pub(crate) fn new(limits: &Limits) -> Arc<Self> {
        Arc::new(Self {
            max_total: limits.max_connections,
            max_per_workspace: limits.max_per_workspace,
            counts: Mutex::new(Counts::default()),
        })
    }

    pub(crate) fn try_acquire(self: &Arc<Self>, workspace: &str) -> Result<Permit, Denied> {
        let mut c = self.lock();
        if c.total >= self.max_total {
            return Err(Denied::Host);
        }
        let open = c.per_workspace.get(workspace).copied().unwrap_or(0);
        if open >= self.max_per_workspace {
            return Err(Denied::Workspace);
        }
        c.total += 1;
        c.per_workspace.insert(workspace.to_owned(), open + 1);
        Ok(Permit { gate: Arc::clone(self), workspace: workspace.to_owned() })
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
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut c = self.gate.lock();
        c.total = c.total.saturating_sub(1);
        if let Some(n) = c.per_workspace.get_mut(&self.workspace) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                c.per_workspace.remove(&self.workspace);
            }
        }
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
        assert_eq!(g.lock().per_workspace.get("a"), None, "empty entries are forgotten");
    }
}
