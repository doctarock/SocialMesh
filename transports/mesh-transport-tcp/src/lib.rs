//! Plain TCP transport. Addresses are `host:port`.
//!
//! NOTE: v0.1 TCP is unencrypted. Object integrity is protected by hashes and
//! signatures, but content and metadata are visible on the network path.
//! See THREAT-MODEL.md.

use mesh_transport::{Accepted, BoxFuture, Listener, Stream, Transport};
use std::io;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

pub const DEFAULT_PORT: u16 = 7842;
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default, Clone, Copy)]
pub struct TcpTransport;

impl Transport for TcpTransport {
    fn name(&self) -> &'static str {
        "tcp"
    }

    fn dial<'a>(&'a self, addr: &'a str) -> BoxFuture<'a, io::Result<Stream>> {
        Box::pin(async move {
            let s = tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect(addr))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timed out"))??;
            s.set_nodelay(true)?;
            Ok(Box::new(s) as Stream)
        })
    }

    fn listen<'a>(&'a self, addr: &'a str) -> BoxFuture<'a, io::Result<Box<dyn Listener>>> {
        Box::pin(async move {
            let l = TcpListener::bind(addr).await?;
            Ok(Box::new(TcpListen(l)) as Box<dyn Listener>)
        })
    }
}

struct TcpListen(TcpListener);

impl Listener for TcpListen {
    fn local_addr(&self) -> String {
        self.0.local_addr().map(|a| a.to_string()).unwrap_or_default()
    }

    fn accept(&mut self) -> BoxFuture<'_, io::Result<Accepted>> {
        Box::pin(async move {
            let (s, remote) = self.0.accept().await?;
            s.set_nodelay(true)?;
            Ok(Accepted { stream: Box::new(s), remote: remote.to_string() })
        })
    }
}
