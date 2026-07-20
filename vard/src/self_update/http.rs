//! The shared HTTP agent for the self-update client.
//!
//! Both the manifest fetch and the tarball download go through one process-wide
//! [`ureq::Agent`] carrying bounded connect and overall timeouts, so a wedged or
//! black-holed connection cannot hang `vard self-update` indefinitely. Cloning an
//! agent is cheap (its connection pool is reference-counted), so callers take a
//! clone per request while sharing the pool and the timeouts.

use std::sync::OnceLock;
use std::time::Duration;

/// Bound on establishing the TCP+TLS connection to the release host, so an
/// unreachable or black-holed endpoint fails fast instead of blocking on connect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Overall bound on a single request (the manifest fetch or the tarball GET), so
/// a connection that stalls mid-transfer cannot hang the command forever. Sized
/// for a multi-megabyte release artifact on a slow link, not for the small
/// manifest.
const CALL_TIMEOUT: Duration = Duration::from_secs(300);

/// The process-wide [`ureq::Agent`] with bounded [`CONNECT_TIMEOUT`] and
/// [`CALL_TIMEOUT`], shared by [`super::manifest`] and [`super::download`].
pub(crate) fn agent() -> ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT
        .get_or_init(|| {
            ureq::AgentBuilder::new()
                .timeout_connect(CONNECT_TIMEOUT)
                .timeout(CALL_TIMEOUT)
                .build()
        })
        .clone()
}
