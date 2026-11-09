//! Transport abstraction (README §19).
//!
//! The object and protocol layers only ever see a bidirectional byte stream.
//! Adding QUIC, Bluetooth, WebRTC, etc. means implementing [`Transport`]; no
//! other layer changes.

use std::future::Future;
use std::io;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncWrite};

/// Any reliable, ordered, bidirectional byte stream.
pub trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}

pub type Stream = Box<dyn Duplex>;
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub struct Accepted {
    pub stream: Stream,
    /// Transport-specific description of the remote end. Advisory only.
    pub remote: String,
}

pub trait Listener: Send {
    /// The address peers can dial, as understood by this transport.
    fn local_addr(&self) -> String;
    fn accept(&mut self) -> BoxFuture<'_, io::Result<Accepted>>;
}

pub trait Transport: Send + Sync {
    fn name(&self) -> &'static str;
    fn dial<'a>(&'a self, addr: &'a str) -> BoxFuture<'a, io::Result<Stream>>;
    fn listen<'a>(&'a self, addr: &'a str) -> BoxFuture<'a, io::Result<Box<dyn Listener>>>;
}
