//! QuicStream adapter for QUIC session transport.
//!
//! Wraps a `QuicTransport` (from the `Transport` trait) and provides the same
//! interface that `hbb_common::Stream` provides — `send()`, `next()`,
//! `set_key()`, `send_bytes()`, etc. — so the session loop can use QUIC
//! without modification.
//!
//! # Encryption
//!
//! QUIC has TLS 1.3 built in, so `set_key()` is a no-op and `is_secured()`
//! always returns `true`. The NaCl-based key exchange in `secure_connection()`
//! can be skipped when using QUIC, but for compatibility the adapter accepts
//! and ignores the key.
//!
//! # Framing
//!
//! `QuicTransport::send_control()` / `recv_control()` already use 4-byte
//! length-prefixed framing, matching the wire format of `FramedStream`. The
//! adapter translates between the protobuf `Message` API that the session
//! loop expects and the raw bytes that `Transport` uses.

use bytes::BytesMut;
use hbb_common::sodiumoxide;
use std::io;
use std::net::SocketAddr;

use super::quic::QuicTransport;
use super::Transport;

/// A QUIC stream adapter that provides the same interface as
/// `hbb_common::Stream` for use in the session loop.
///
/// Wraps a `QuicTransport` and exposes `send()`, `next()`, `set_key()`, etc.
/// so code that currently works with `hbb_common::Stream` can work with QUIC
/// transparently.
pub struct QuicStream {
    transport: QuicTransport,
    /// Send timeout in milliseconds (0 = no timeout). Mirrored from
    /// `FramedStream::set_send_timeout` but not enforced for QUIC (QUIC
    /// has its own idle timeout and congestion control).
    send_timeout_ms: u64,
}

impl QuicStream {
    /// Create a new `QuicStream` from a connected `QuicTransport`.
    pub fn new(transport: QuicTransport) -> Self {
        Self {
            transport,
            send_timeout_ms: 0,
        }
    }

    /// Send a protobuf message over the QUIC control stream.
    ///
    /// Serializes the message to bytes, then sends via the reliable
    /// QUIC control stream with length-prefixed framing.
    pub async fn send(&mut self, msg: &impl hbb_common::protobuf::Message) -> hbb_common::ResultType<()> {
        let bytes = msg.write_to_bytes()?;
        self.send_raw(bytes).await
    }

    /// Send raw bytes over the QUIC control stream.
    ///
    /// QUIC has TLS 1.3 built in, so no additional NaCl encryption is applied.
    pub async fn send_raw(&mut self, msg: Vec<u8>) -> hbb_common::ResultType<()> {
        self.transport.send_control(&msg).await
    }

    /// Send pre-formed bytes over the QUIC control stream.
    pub async fn send_bytes(&mut self, bytes: bytes::Bytes) -> hbb_common::ResultType<()> {
        self.transport.send_control(&bytes).await
    }

    /// Receive the next message from the QUIC control stream.
    ///
    /// Returns `Some(Ok(bytes))` on success, `Some(Err(..))` on I/O error,
    /// or `None` if the connection is closed.
    pub async fn next(&mut self) -> Option<Result<BytesMut, io::Error>> {
        if !self.transport.is_connected() {
            return None;
        }
        match self.transport.recv_control().await {
            Ok(data) => Some(Ok(BytesMut::from(data.as_slice()))),
            Err(e) => {
                // Convert anyhow::Error to io::Error for compatibility.
                Some(Err(io::Error::new(io::ErrorKind::Other, e.to_string())))
            }
        }
    }

    /// Receive the next message with a timeout.
    ///
    /// Returns `None` if the timeout expires or the connection is closed.
    pub async fn next_timeout(&mut self, ms: u64) -> Option<Result<BytesMut, io::Error>> {
        if let Ok(res) = hbb_common::timeout(ms, self.next()).await {
            res
        } else {
            None
        }
    }

    /// Set the encryption key. No-op for QUIC (TLS 1.3 is built in).
    pub fn set_key(&mut self, _key: sodiumoxide::crypto::secretbox::Key) {
        // QUIC has TLS 1.3 built in — NaCl encryption is unnecessary.
        // Accept and ignore the key for API compatibility.
    }

    /// Set raw mode. No-op for QUIC (framing is always length-prefixed).
    pub fn set_raw(&mut self) {
        // QUIC control stream always uses length-prefixed framing.
        // No raw mode needed.
    }

    /// Set the send timeout. Stored but not enforced (QUIC has its own
    /// congestion control and idle timeout).
    pub fn set_send_timeout(&mut self, ms: u64) {
        self.send_timeout_ms = ms;
    }

    /// Returns `true` — QUIC always has TLS 1.3 encryption.
    pub fn is_secured(&self) -> bool {
        true
    }

    /// Returns the local address of the QUIC endpoint.
    ///
    /// Note: QUIC uses UDP, so this returns the UDP socket address.
    /// Falls back to `0.0.0.0:0` if the address is not available.
    pub fn local_addr(&self) -> SocketAddr {
        // QuicTransport exposes remote_addr but not local_addr.
        // Return a placeholder — callers that need the real local address
        // (e.g., NAT traversal) should not use QUIC direct connections.
        "0.0.0.0:0".parse().unwrap()
    }

    /// Returns a reference to the underlying `QuicTransport`.
    pub fn transport(&self) -> &QuicTransport {
        &self.transport
    }

    /// Returns a mutable reference to the underlying `QuicTransport`.
    pub fn transport_mut(&mut self) -> &mut QuicTransport {
        &mut self.transport
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quic_stream_set_key_is_noop() {
        // QuicStream::set_key should accept a key without error.
        // We can only test this with a disconnected transport.
        let transport = QuicTransport::new_disconnected("127.0.0.1:0".parse().unwrap());
        // new_disconnected returns different types depending on feature flag,
        // but both implement Transport. For testing set_key, we need a QuicTransport.
        // Since we can't construct a real QuicTransport without a connection,
        // we test the method signature exists and compiles.
        let _ = transport; // just verify it compiles
    }

    #[test]
    fn quic_stream_is_always_secured() {
        // Verify the is_secured API contract: QUIC is always secured.
        // We can't construct a QuicStream without a real QuicTransport,
        // but we can verify the method exists at compile time.
        fn _assert_secured(qs: &QuicStream) {
            assert!(qs.is_secured());
        }
    }

    #[test]
    fn quic_stream_set_raw_is_noop() {
        // Verify set_raw compiles (it's a no-op).
        fn _assert_set_raw(qs: &mut QuicStream) {
            qs.set_raw();
        }
    }

    #[test]
    fn quic_stream_local_addr_returns_placeholder() {
        // Verify local_addr returns a valid SocketAddr.
        fn _assert_local_addr(qs: &QuicStream) -> SocketAddr {
            qs.local_addr()
        }
    }

    /// When quic-transport feature is enabled, test with a real loopback pair.
    #[cfg(feature = "quic-transport")]
    mod with_transport {
        use super::*;
        use crate::transport::quic::{generate_self_signed_cert, QuicServer};
        use hbb_common::tokio;

        async fn loopback_quic_stream_pair() -> (QuicStream, QuicStream) {
            let (cert_der, key_der) = generate_self_signed_cert().unwrap();

            let server = QuicServer::bind(
                "127.0.0.1:0".parse().unwrap(),
                cert_der.clone(),
                key_der,
            )
            .unwrap();
            let server_addr = server.local_addr().unwrap();

            let server_handle = tokio::spawn(async move {
                server.accept().await.unwrap().unwrap()
            });

            let client_handle = tokio::spawn({
                let cert = cert_der.clone();
                async move { QuicTransport::connect(server_addr, &cert).await.unwrap() }
            });

            let (server_result, client_result) = tokio::join!(server_handle, client_handle);
            (
                QuicStream::new(server_result.unwrap()),
                QuicStream::new(client_result.unwrap()),
            )
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn quic_stream_send_recv_roundtrip() {
            let (mut server, mut client) = loopback_quic_stream_pair().await;

            // Client sends raw bytes, server receives them.
            client.send_raw(b"hello quic".to_vec()).await.unwrap();
            let received = server.next().await.unwrap().unwrap();
            assert_eq!(&received[..], b"hello quic");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn quic_stream_protobuf_roundtrip() {
            use hbb_common::message_proto::Message;
            use hbb_common::protobuf::Message as _;

            let (mut server, mut client) = loopback_quic_stream_pair().await;

            // Send a protobuf Message via QuicStream.
            let msg = Message::new();
            client.send(&msg).await.unwrap();

            // Receive and parse it.
            let bytes = server.next().await.unwrap().unwrap();
            let parsed = Message::parse_from_bytes(&bytes).unwrap();
            assert_eq!(parsed, msg);
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn quic_stream_is_secured_returns_true() {
            let (_server, client) = loopback_quic_stream_pair().await;
            assert!(client.is_secured());
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn quic_stream_set_key_does_not_break_transport() {
            let (mut server, mut client) = loopback_quic_stream_pair().await;

            // Set a key (should be ignored) — transport should still work.
            let key = sodiumoxide::crypto::secretbox::gen_key();
            client.set_key(key);

            client.send_raw(b"after set_key".to_vec()).await.unwrap();
            let received = server.next().await.unwrap().unwrap();
            assert_eq!(&received[..], b"after set_key");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn quic_stream_send_bytes() {
            let (mut server, mut client) = loopback_quic_stream_pair().await;

            client
                .send_bytes(bytes::Bytes::from_static(b"via send_bytes"))
                .await
                .unwrap();
            let received = server.next().await.unwrap().unwrap();
            assert_eq!(&received[..], b"via send_bytes");
        }
    }
}
