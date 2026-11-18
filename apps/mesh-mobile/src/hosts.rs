//! Hosts: nodes this phone dials by address (a computer at home, a relay
//! someone runs), as opposed to phones found by radio. Saved hosts are
//! redialled for as long as the node runs; hosts announcing themselves on
//! the local network are listed so they can be saved for later.

use crate::{push_json_str, Mobile};
use anyhow::{bail, Result};
use mesh_crypto::Keypair;
use mesh_discovery::invite::{Invitation, PREFIX};
use mesh_transport_tcp::{TcpTransport, DEFAULT_PORT};
use std::fmt::Write as _;
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// `host`, `host:port` or `tcp://host:port` as `host:port`.
pub fn normalise(addr: &str) -> Result<String> {
    let a = addr.trim();
    let a = a.strip_prefix("tcp://").unwrap_or(a).trim_end_matches('/');
    if a.is_empty() || a.len() > 255 || a.chars().any(|c| c.is_whitespace() || c == '@' || c == '/') {
        bail!("that doesn't look like an address (try 192.168.1.20 or relay.example.org:7842)");
    }
    if a.starts_with("radio:") {
        bail!("radio addresses change; phones nearby are found by themselves");
    }
    let has_port = a.rsplit_once(':').is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok_and(|p| p > 0));
    // A bare IPv6 address has colons but no port.
    let bare_v6 = a.matches(':').count() > 1 && !a.starts_with('[');
    Ok(if has_port && !bare_v6 { a.to_string() } else if bare_v6 { format!("[{a}]:{DEFAULT_PORT}") } else { format!("{a}:{DEFAULT_PORT}") })
}

/// Sightings from discovery that are network addresses (not radio ones).
fn is_network(addr: &str) -> bool {
    !addr.starts_with("radio:")
}

impl Mobile {
    /// Saved hosts and hosts announcing themselves on this network:
    /// `{"saved":[{"addr","id","last_seen_ms","linked"}],"found":[{"addr","id","linked"}]}`.
    /// `id` is a short identity or null; `last_seen_ms` is null if never connected.
    pub fn hosts_json(&self) -> Result<String> {
        let saved = self.node.with_store(|s| s.peers())?;
        let mut out = String::from("{\"saved\":[");
        for (i, p) in saved.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"addr\":");
            push_json_str(&mut out, &p.addr);
            match p.identity {
                Some(d) => {
                    let _ = write!(out, ",\"id\":\"{}\"", d.short());
                }
                None => out.push_str(",\"id\":null"),
            }
            match p.last_seen_ms {
                Some(t) => {
                    let _ = write!(out, ",\"last_seen_ms\":{t}");
                }
                None => out.push_str(",\"last_seen_ms\":null"),
            }
            let _ = write!(out, ",\"linked\":{}}}", self.node.is_linked(&p.addr));
        }
        out.push_str("],\"found\":[");
        let found = self.node.discovered();
        let found = found.iter().filter(|(a, _)| is_network(a) && !saved.iter().any(|p| &p.addr == a));
        for (i, (addr, id)) in found.enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"addr\":");
            push_json_str(&mut out, addr);
            match id {
                Some(d) => {
                    let _ = write!(out, ",\"id\":\"{}\"", d.short());
                }
                None => out.push_str(",\"id\":null"),
            }
            let _ = write!(out, ",\"linked\":{}}}", self.node.is_linked(addr));
        }
        out.push_str("]}");
        Ok(out)
    }

    /// Save a host and connect to it: an address (`host[:port]`) or a
    /// `mesh1:` invitation. The identity that answers is pinned, so someone
    /// else taking over the address later is refused. Returns a line for
    /// the person; errors if the input isn't usable or the identity is wrong.
    pub fn add_host(&self, input: &str) -> Result<String> {
        // Probes use a throwaway key: they never show this node's link key.
        let probe_key = Keypair::generate();
        let _rt = self.rt.enter(); // dialling spawns onto the node's runtime
        if input.contains(PREFIX) {
            let inv = Invitation::parse(input)?;
            if inv.addrs.is_empty() {
                bail!("that invitation has no address in it");
            }
            let mut lines = Vec::new();
            for a in &inv.addrs {
                match self.rt.block_on(mesh_node::probe(&TcpTransport, &probe_key, a, PROBE_TIMEOUT)) {
                    Ok(pk) if pk.id() != inv.identity => {
                        lines.push(format!("Refused {a}: it isn't the node in the invitation"));
                        continue;
                    }
                    Ok(_) => lines.push(format!("Connected to {} at {a}", inv.identity.short())),
                    Err(_) => lines.push(format!("Saved {a}; can't reach it now, will keep trying")),
                }
                self.node.with_store(|s| s.add_peer(a, Some(&inv.identity)))?;
                self.node.dial(a, Some(inv.identity));
            }
            return Ok(lines.join("\n"));
        }
        let addr = normalise(input)?;
        match self.rt.block_on(mesh_node::probe(&TcpTransport, &probe_key, &addr, PROBE_TIMEOUT)) {
            Ok(pk) => {
                self.node.with_store(|s| s.add_peer(&addr, Some(&pk.id())))?;
                self.node.dial(&addr, Some(pk.id()));
                Ok(format!("Connected to {} at {addr}", pk.id().short()))
            }
            Err(_) => {
                self.node.with_store(|s| s.add_peer(&addr, None))?;
                self.node.dial(&addr, None);
                Ok(format!("Saved {addr}; can't reach it now, will keep trying"))
            }
        }
    }

    /// Forget a saved host. `false` if it wasn't saved.
    pub fn remove_host(&self, addr: &str) -> Result<bool> {
        Ok(self.node.with_store(|s| s.remove_peer(addr))?)
    }
}

#[cfg(test)]
mod tests {
    use super::normalise;

    #[test]
    fn addresses_get_the_default_port() {
        assert_eq!(normalise(" 192.168.1.20 ").unwrap(), "192.168.1.20:7842");
        assert_eq!(normalise("tcp://relay.example.org:9000/").unwrap(), "relay.example.org:9000");
        assert_eq!(normalise("fe80::1").unwrap(), "[fe80::1]:7842");
        assert_eq!(normalise("[fe80::1]:9").unwrap(), "[fe80::1]:9");
        for bad in ["", "a b", "radio:abc", "http://x/y", "x@y"] {
            assert!(normalise(bad).is_err(), "{bad}");
        }
    }
}
