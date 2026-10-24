use std::time::{Duration, Instant};

use futures::FutureExt as _;
use gpui::BackgroundExecutor;
use smol::net::TcpStream;

/// How long a single TCP-connect probe waits before declaring a timeout.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// The default port for probing a Proxmox node (its web/API port).
pub const NODE_PROBE_PORT: u16 = 8006;
/// The default port for probing a container host (SSH).
pub const HOST_PROBE_PORT: u16 = 22;

/// The reachability of a target, as measured by a TCP-connect probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reachability {
    /// The target accepted a TCP connection; the value is the connect latency.
    Reachable(Duration),
    /// The target did not answer within the timeout (or refused the connection).
    Timeout,
    /// There is no probeable endpoint for this target.
    NotApplicable,
}

/// Probes TCP reachability to `host:port`, returning the connect latency.
///
/// Deterministic: a success returns the measured round trip; a connect failure
/// or a host that does not answer within [`PROBE_TIMEOUT`] returns `Timeout`;
/// an empty host returns `NotApplicable`.
pub async fn probe_tcp(executor: &BackgroundExecutor, host: &str, port: u16) -> Reachability {
    let host = host.trim();
    if host.is_empty() {
        return Reachability::NotApplicable;
    }

    let started = Instant::now();
    let connect = async { TcpStream::connect((host, port)).await.map(|_stream| ()) }.fuse();
    let timeout = executor.timer(PROBE_TIMEOUT).fuse();
    futures::pin_mut!(connect, timeout);

    futures::select! {
        result = connect => match result {
            Ok(()) => Reachability::Reachable(started.elapsed()),
            Err(_) => Reachability::Timeout,
        },
        _ = timeout => Reachability::Timeout,
    }
}

/// Strips a `user@` prefix from an SSH-style endpoint, returning the bare host.
pub fn bare_host(endpoint: &str) -> &str {
    match endpoint.rsplit_once('@') {
        Some((_, host)) => host,
        None => endpoint,
    }
}
