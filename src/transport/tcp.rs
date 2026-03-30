//! TCP transport implementation.
//!
//! Wraps a standard TCP connection as a [`Transport`](super::Transport) implementation.
//! This is the current default transport for RustDesk — all existing connections
//! use TCP (or WebSocket) under the hood. By implementing the `Transport` trait,
//! TCP connections become interchangeable with QUIC connections at the call site.
//!
//! # Framing
//!
//! Messages are length-prefixed with a 4-byte big-endian length header,
//! matching the existing `hbb_common::tcp::FramedStream` wire format.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use hbb_common::tokio::io::{AsyncReadExt, AsyncWriteExt};
use hbb_common::tokio::net::TcpStream;
use hbb_common::tokio::sync::Mutex;
use hbb_common::ResultType;

use super::Transport;

/// A TCP-based transport for RustDesk peer connections.
///
/// Wraps a `tokio::net::TcpStream` split into read/write halves behind
/// async mutexes so the transport can be shared across tasks.
///
/// Both video and control data are sent over the same TCP stream using
/// length-prefixed framing. Unlike QUIC, there is no distinction between
/// reliable and unreliable delivery — everything is reliable and ordered.
pub struct TcpTransport {
    reader: Mutex<hbb_common::tokio::net::tcp::OwnedReadHalf>,
    writer: Mutex<hbb_common::tokio::net::tcp::OwnedWriteHalf>,
    connected: Arc<AtomicBool>,
}

impl TcpTransport {
    /// Create a new `TcpTransport` from an already-connected `TcpStream`.
    ///
    /// The stream is split into independent read and write halves so that
    /// sends and receives can happen concurrently.
    pub fn new(stream: TcpStream) -> Self {
        let (reader, writer) = stream.into_split();
        Self {
            reader: Mutex::new(reader),
            writer: Mutex::new(writer),
            connected: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Send a length-prefixed message over the TCP stream.
    ///
    /// Wire format: `[4-byte big-endian length][payload]`
    async fn send_framed(&self, data: &[u8]) -> ResultType<()> {
        let mut writer = self.writer.lock().await;
        let len = (data.len() as u32).to_be_bytes();
        writer.write_all(&len).await?;
        writer.write_all(data).await?;
        writer.flush().await?;
        Ok(())
    }

    /// Receive a length-prefixed message from the TCP stream.
    ///
    /// Reads a 4-byte big-endian length, then reads that many bytes.
    async fn recv_framed(&self) -> ResultType<Vec<u8>> {
        let mut reader = self.reader.lock().await;
        let mut len_buf = [0u8; 4];
        reader.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).await?;
        Ok(buf)
    }
}

#[async_trait]
impl Transport for TcpTransport {
    async fn send_video(&self, data: &[u8]) -> ResultType<()> {
        self.send_framed(data).await
    }

    async fn recv_video(&self) -> ResultType<Vec<u8>> {
        self.recv_framed().await
    }

    async fn send_control(&self, data: &[u8]) -> ResultType<()> {
        self.send_framed(data).await
    }

    async fn recv_control(&self) -> ResultType<Vec<u8>> {
        self.recv_framed().await
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    fn transport_type(&self) -> &str {
        "tcp"
    }
}
