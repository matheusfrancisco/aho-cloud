//! Port leasing (executor.md "Ports"): `RunnerState::lease_ports` picks the lowest free ports,
//! each is bind-tested on `127.0.0.1` with `SO_REUSEADDR` off, and a port that fails goes to
//! quarantine for 10 minutes before leasing again.

use std::net::{Ipv4Addr, SocketAddr};

use super::HostShared;
use crate::ports::executor::{ExecError, PortName};
use crate::ports::runner_state::{PortLease, PortOwner, PortRange};

const QUARANTINE: time::Duration = time::Duration::minutes(10);
const MAX_ROUNDS: usize = 32;

/// Whether `127.0.0.1:port` can be bound right now.
pub(crate) fn bind_test(port: u16) -> bool {
    let Ok(sock) = tokio::net::TcpSocket::new_v4() else { return false };
    if sock.set_reuseaddr(false).is_err() {
        return false;
    }
    sock.bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).is_ok() && sock.listen(1).is_ok()
}

pub(crate) fn pool(host: &HostShared) -> PortRange {
    let [start, end] = host.cfg.ports.range;
    PortRange { start, end }
}

/// Leases one bind-tested port per entry of `names`, in order.
pub(crate) async fn lease(host: &HostShared, owner: &PortOwner, names: &[Option<PortName>]) -> Result<Vec<PortLease>, ExecError> {
    let mut slots: Vec<Option<PortLease>> = vec![None; names.len()];
    for _ in 0..MAX_ROUNDS {
        let missing: Vec<usize> = (0..names.len()).filter(|&i| slots[i].is_none()).collect();
        if missing.is_empty() {
            break;
        }
        let wanted: Vec<Option<PortName>> = missing.iter().map(|&i| names[i].clone()).collect();
        let leases = host.state.lease_ports(owner, &wanted, pool(host)).await?;
        for (idx, lease) in missing.into_iter().zip(leases) {
            if bind_test(lease.port) {
                slots[idx] = Some(lease);
            } else {
                host.state.quarantine_port(lease.port, host.clock.now() + QUARANTINE).await?;
            }
        }
    }
    let leases: Vec<PortLease> = slots.into_iter().flatten().collect();
    if leases.len() != names.len() {
        return Err(ExecError::Unavailable("no bindable port in runner.ports.range".into()));
    }
    Ok(leases)
}

/// Moves `ports` to quarantine (a service failed with `EADDRINUSE` on them).
pub(crate) async fn quarantine(host: &HostShared, ports: &[u16]) -> Result<(), ExecError> {
    for port in ports {
        host.state.quarantine_port(*port, host.clock.now() + QUARANTINE).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bind_test_detects_a_taken_port() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!bind_test(port));
        drop(listener);
        // Tests running in parallel open sockets on ephemeral ports, so a freed port can be taken
        // again before the check; at least one of a few freed ports must test bindable.
        let freed = (0..5).any(|_| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            bind_test(port)
        });
        assert!(freed);
    }
}
