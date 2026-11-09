//! `mesh1:` invitations (README §22).
//!
//! ```text
//! mesh1:<identity-id-hex>[@addr[,addr...]]
//! mesh1:01a91f...c3@203.0.113.7:7842,192.168.1.20:7842
//! ```
//!
//! One string that works everywhere: paste it in chat, put it in a URL or QR
//! code, write it to an NFC tag, or chirp it over audio. The identity is
//! *pinned*: when dialling an invitation the node rejects any peer whose
//! handshake proves a different key, so a hostile network can redirect the
//! connection but cannot impersonate the invited peer.
//!
//! Addresses are hints and may be stale; the identity is what matters.

use mesh_crypto::Digest;

pub const PREFIX: &str = "mesh1:";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invitation {
    pub identity: Digest,
    pub addrs: Vec<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InviteError {
    #[error("not a mesh1: invitation")]
    Prefix,
    #[error("invalid identity in invitation")]
    Identity,
    #[error("invalid address `{0}` in invitation")]
    Addr(String),
}

impl Invitation {
    pub fn parse(s: &str) -> Result<Self, InviteError> {
        let s = s.trim();
        // Accept web-style wrappers such as https://host/#mesh1:... or ?invite=mesh1:...
        let body = s.find(PREFIX).map(|i| &s[i + PREFIX.len()..]).ok_or(InviteError::Prefix)?;
        let (id, addrs) = match body.split_once('@') {
            Some((id, a)) => (id, a),
            None => (body, ""),
        };
        let identity = Digest::from_hex(id).map_err(|_| InviteError::Identity)?;
        let mut out = Vec::new();
        for a in addrs.split(',').filter(|a| !a.is_empty()) {
            if a.len() > 255 || a.chars().any(|c| c.is_whitespace() || c == '@') || !a.contains(':') {
                return Err(InviteError::Addr(a.into()));
            }
            out.push(a.to_string());
        }
        if out.len() > 16 {
            out.truncate(16);
        }
        Ok(Invitation { identity, addrs: out })
    }
}

impl std::fmt::Display for Invitation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{PREFIX}{}", self.identity.to_hex())?;
        if !self.addrs.is_empty() {
            write!(f, "@{}", self.addrs.join(","))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_wrapped() {
        let i = Invitation { identity: Digest::of(b"id"), addrs: vec!["1.2.3.4:7842".into(), "[::1]:7842".into()] };
        let s = i.to_string();
        assert_eq!(Invitation::parse(&s).unwrap(), i);
        assert_eq!(Invitation::parse(&format!("https://example.org/join#{s}")).unwrap(), i);
        let bare = Invitation { identity: i.identity, addrs: vec![] };
        assert_eq!(Invitation::parse(&bare.to_string()).unwrap(), bare);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(Invitation::parse("hello"), Err(InviteError::Prefix));
        assert_eq!(Invitation::parse("mesh1:zz"), Err(InviteError::Identity));
        let id = Digest::of(b"x").to_hex();
        assert!(matches!(Invitation::parse(&format!("mesh1:{id}@no port")), Err(InviteError::Addr(_))));
    }
}
