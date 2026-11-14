//! HTTP and HTTPS, with names looked up in the DNS servers the network
//! gave.

use crate::dns;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

/// The DNS servers to ask, as the network last gave them.
pub type Servers = Arc<Mutex<Vec<Ipv4Addr>>>;

/// How long reaching a server may take.
const CONNECT: Duration = Duration::from_secs(15);

/// How long a connection may go without a byte either way before it is
/// taken as lost: a download read as it comes has no end to wait for.
const STALL: Duration = Duration::from_secs(30);

/// How long a mirror latency probe may take to reach and answer.
const PROBE_CONNECT: Duration = Duration::from_secs(4);
const PROBE_STALL: Duration = Duration::from_secs(4);

#[derive(Debug)]
struct Lookup(Servers);

impl Resolver for Lookup {
    fn resolve(&self, uri: &Uri, _config: &Config, _timeout: NextTimeout) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let host = uri.host().ok_or_else(|| ureq::Error::BadUri(format!("{uri} names no host")))?;
        let port = uri.port_u16().unwrap_or(if uri.scheme_str() == Some("http") { 80 } else { 443 });
        let servers = self.0.lock().expect("the DNS servers are not held by a thread that panicked").clone();
        let found = dns::lookup(host, &servers).map_err(|e| ureq::Error::Io(std::io::Error::other(format!("{e:#}"))))?;
        let mut out = self.empty();
        // As many as it holds; the rest are not tried.
        for address in found {
            if out.try_push(SocketAddr::new(IpAddr::V4(address), port)).is_err() {
                break;
            }
        }
        Ok(out)
    }
}

/// An agent that looks names up in `servers`, and gives a server's answer
/// whatever its status: the caller says what a status means.
pub fn agent(servers: Servers) -> ureq::Agent {
    with_timeouts(servers, CONNECT, STALL)
}

/// An agent for the short question of a mirror's latency.
pub fn probe_agent(servers: Servers) -> ureq::Agent {
    with_timeouts(servers, PROBE_CONNECT, PROBE_STALL)
}

fn with_timeouts(servers: Servers, connect: Duration, stall: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .proxy(None)
        .timeout_connect(Some(connect))
        .timeout_per_read(Some(stall))
        .timeout_per_write(Some(stall))
        .http_status_as_error(false)
        .build();
    ureq::Agent::with_parts(config, DefaultConnector::new(), Lookup(servers))
}
