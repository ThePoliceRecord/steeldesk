//! QUIC transport layer for RustDesk connections.
//!
//! Uses the `quinn` crate to provide:
//! - Unreliable datagram delivery for video/audio frames (QUIC DATAGRAM
//!   extension, RFC 9221).
//! - Reliable bidirectional streams for control messages, file transfers,
//!   and clipboard data.
//! - Built-in encryption (TLS 1.3), congestion control (BBR), and 0-RTT
//!   reconnection.
//!
//! # Connection Model
//!
//! A single QUIC connection multiplexes several logical channels:
//!
//! | Channel   | QUIC primitive       | Reliability |
//! |-----------|----------------------|-------------|
//! | Video     | Datagram             | Unreliable  |
//! | Audio     | Datagram             | Unreliable  |
//! | Control   | Bidirectional stream | Reliable    |
//! | File xfer | Unidirectional stream| Reliable    |
//! | Port fwd  | Bidirectional stream | Reliable    |
//!
//! # Feature Gate
//!
//! The real implementation is behind `#[cfg(feature = "quic-transport")]`.
//! Without the feature, only `todo!()` stubs and `new_disconnected()` are
//! available (enough to compile and test transport selection logic).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use hbb_common::ResultType;

use super::Transport;

// ──────────────────────────────────────────────────────────────────────
// QUIC certificate cache (available regardless of quic-transport feature)
// ──────────────────────────────────────────────────────────────────────

lazy_static::lazy_static! {
    /// Cached self-signed QUIC certificate and private key `(cert_der, key_der)`.
    ///
    /// Generated once on first access and reused for the lifetime of the
    /// process. Both peers exchange their `cert_der` during the rendezvous
    /// handshake so the QUIC TLS handshake can verify the remote endpoint.
    static ref QUIC_CERT: RwLock<Option<(Vec<u8>, Vec<u8>)>> = RwLock::new(None);
}

/// Get or generate the cached self-signed QUIC certificate.
///
/// Returns `(cert_der, key_der)`. The certificate is generated once and
/// cached for the lifetime of the process. Thread-safe.
///
/// # Panics
///
/// Panics if `rcgen` cert generation fails (should not happen with the
/// default "rustdesk" subject name) or if the RwLock is poisoned.
#[cfg(feature = "quic-transport")]
pub fn get_or_generate_quic_cert() -> (Vec<u8>, Vec<u8>) {
    // Fast path: cert already generated.
    {
        let guard = QUIC_CERT.read().unwrap();
        if let Some(ref cert) = *guard {
            return cert.clone();
        }
    }
    // Slow path: generate and cache.
    let mut guard = QUIC_CERT.write().unwrap();
    // Double-check after acquiring write lock.
    if let Some(ref cert) = *guard {
        return cert.clone();
    }
    let cert = inner::generate_self_signed_cert()
        .expect("failed to generate self-signed QUIC certificate");
    *guard = Some(cert.clone());
    cert
}

/// Stub version for builds without `quic-transport`.
///
/// Returns an empty `(cert_der, key_der)` pair. The empty cert will
/// cause `try_quic_connection` to skip the QUIC attempt gracefully.
#[cfg(not(feature = "quic-transport"))]
pub fn get_or_generate_quic_cert() -> (Vec<u8>, Vec<u8>) {
    (Vec::new(), Vec::new())
}

/// Get the DER-encoded self-signed certificate for QUIC transport.
///
/// This is the certificate that should be sent to peers during the
/// rendezvous handshake. The peer uses it to verify our QUIC TLS endpoint.
pub fn get_quic_cert_der() -> Vec<u8> {
    get_or_generate_quic_cert().0
}

/// Clear the cached QUIC certificate (for testing).
#[cfg(test)]
pub fn clear_quic_cert_cache() {
    let mut guard = QUIC_CERT.write().unwrap();
    *guard = None;
}

// ──────────────────────────────────────────────────────────────────────
// Real implementation (feature = "quic-transport")
// ──────────────────────────────────────────────────────────────────────
#[cfg(feature = "quic-transport")]
mod inner {
    use super::*;
    use hbb_common::bytes::Bytes;
    use hbb_common::tokio::sync::Mutex;
    use quinn::{Connection, RecvStream, SendStream};
    use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
    use std::time::Duration;

    /// A QUIC-based transport for RustDesk peer connections.
    ///
    /// Wraps a single QUIC connection to a remote peer, exposing separate
    /// methods for unreliable datagram delivery (video/audio) and reliable
    /// stream delivery (control, files).
    pub struct QuicTransport {
        /// Remote peer address.
        remote_addr: SocketAddr,

        /// Whether the connection is currently alive.
        connected: Arc<AtomicBool>,

        /// The QUIC endpoint that owns the UDP socket and drives I/O.
        /// Must be kept alive for the duration of the connection; dropping
        /// the last endpoint clone shuts down the background I/O driver.
        #[allow(dead_code)]
        endpoint: quinn::Endpoint,

        /// The underlying QUIC connection handle.
        connection: Connection,

        /// Write half of the control bidirectional stream.
        control_send: Mutex<SendStream>,

        /// Read half of the control bidirectional stream.
        control_recv: Mutex<RecvStream>,
    }

    impl QuicTransport {
        /// Establish a QUIC connection to the given address.
        ///
        /// `server_cert` is the DER-encoded certificate of the remote peer,
        /// used to verify the TLS handshake. In production this will come from
        /// the rendezvous server's key exchange or a pre-shared certificate.
        ///
        /// # Transport Configuration
        ///
        /// The connection is configured with:
        /// - BBR congestion control (better than CUBIC for real-time video)
        /// - DATAGRAM extension enabled (for unreliable video/audio delivery)
        /// - Keep-alive interval of 5 seconds
        /// - Idle timeout of 30 seconds
        ///
        /// # Errors
        ///
        /// Returns an error if the QUIC handshake fails, the certificate is
        /// invalid, or the remote endpoint is unreachable.
        pub async fn connect(addr: SocketAddr, server_cert: &[u8]) -> ResultType<Self> {
            // Build client config that trusts the given server certificate.
            let cert = CertificateDer::from(server_cert.to_vec());
            let mut root_store = rustls::RootCertStore::empty();
            root_store.add(cert)?;
            let mut client_config =
                quinn::ClientConfig::with_root_certificates(Arc::new(root_store))?;
            client_config.transport_config(Arc::new(build_transport_config(true)));

            let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
            endpoint.set_default_client_config(client_config);

            let connection = endpoint.connect(addr, "rustdesk")?.await?;

            // Open a bidirectional stream for the control channel.
            // We must send at least one byte so the server's accept_bi()
            // can see the stream (QUIC only transmits stream creation
            // when there is data to send).
            let (mut send, recv) = connection.open_bi().await?;
            send.write_all(&[0u8]).await?;

            Ok(Self {
                remote_addr: addr,
                connected: Arc::new(AtomicBool::new(true)),
                endpoint,
                connection,
                control_send: Mutex::new(send),
                control_recv: Mutex::new(recv),
            })
        }

        /// Create a `QuicTransport` from an already-accepted `quinn::Connection`.
        ///
        /// Used by `QuicServer::accept()` on the server side. The `endpoint`
        /// must be kept alive to drive I/O for the connection.
        pub(crate) async fn from_connection(
            endpoint: quinn::Endpoint,
            connection: Connection,
        ) -> ResultType<Self> {
            let remote_addr = connection.remote_address();

            // Accept the bidirectional control stream opened by the client.
            // The client sends a single byte to trigger stream creation
            // on the wire; read and discard it.
            let (send, mut recv) = connection.accept_bi().await?;
            let mut _handshake = [0u8; 1];
            recv.read_exact(&mut _handshake).await?;

            Ok(Self {
                remote_addr,
                connected: Arc::new(AtomicBool::new(true)),
                endpoint,
                connection,
                control_send: Mutex::new(send),
                control_recv: Mutex::new(recv),
            })
        }

        /// Create a disconnected `QuicTransport` for testing purposes.
        ///
        /// The returned transport has `is_connected() == false` and will panic
        /// if any send/recv methods are called. This is only useful for verifying
        /// trait implementations and transport selection logic in tests.
        pub fn new_disconnected(addr: SocketAddr) -> DisconnectedQuicTransport {
            DisconnectedQuicTransport {
                remote_addr: addr,
                connected: Arc::new(AtomicBool::new(false)),
            }
        }

        /// Send a video frame as an unreliable QUIC datagram.
        ///
        /// Falls back to a unidirectional stream if the connection does not
        /// support datagrams.
        pub async fn send_video_frame(&self, frame: &[u8]) -> ResultType<()> {
            if self.connection.max_datagram_size().is_some() {
                self.connection
                    .send_datagram(Bytes::copy_from_slice(frame))?;
            } else {
                // Fallback: open a unidirectional stream per frame.
                let mut send = self.connection.open_uni().await?;
                send.write_all(frame).await?;
                send.finish()?;
            }
            Ok(())
        }

        /// Receive a video frame datagram from the remote peer.
        ///
        /// Returns the raw bytes of a single datagram. Falls back to reading
        /// from a unidirectional stream if datagrams are not supported.
        pub async fn recv_video_frame(&self) -> ResultType<Vec<u8>> {
            if self.connection.max_datagram_size().is_some() {
                let datagram = self.connection.read_datagram().await?;
                Ok(datagram.to_vec())
            } else {
                // Fallback: accept a unidirectional stream.
                let mut recv = self.connection.accept_uni().await?;
                let data = recv.read_to_end(16 * 1024 * 1024).await?;
                Ok(data)
            }
        }

        /// Returns the remote peer's socket address.
        pub fn remote_addr(&self) -> SocketAddr {
            self.remote_addr
        }

        /// Returns the maximum datagram size supported by this connection.
        pub fn max_datagram_size(&self) -> Option<usize> {
            self.connection.max_datagram_size()
        }

        /// Gracefully close the QUIC connection.
        pub fn close(&self, reason: &str) {
            self.connected.store(false, Ordering::Relaxed);
            self.connection
                .close(quinn::VarInt::from_u32(0), reason.as_bytes());
        }
    }

    #[async_trait]
    impl Transport for QuicTransport {
        async fn send_video(&self, data: &[u8]) -> ResultType<()> {
            self.send_video_frame(data).await
        }

        async fn recv_video(&self) -> ResultType<Vec<u8>> {
            self.recv_video_frame().await
        }

        async fn send_control(&self, data: &[u8]) -> ResultType<()> {
            let mut writer = self.control_send.lock().await;
            let len = (data.len() as u32).to_be_bytes();
            writer.write_all(&len).await?;
            writer.write_all(data).await?;
            Ok(())
        }

        async fn recv_control(&self) -> ResultType<Vec<u8>> {
            let mut reader = self.control_recv.lock().await;
            let mut len_buf = [0u8; 4];
            reader.read_exact(&mut len_buf).await?;
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf).await?;
            Ok(buf)
        }

        fn is_connected(&self) -> bool {
            self.connected.load(Ordering::Relaxed)
        }

        fn transport_type(&self) -> &str {
            "quic"
        }
    }

    // ── DisconnectedQuicTransport (for testing without a live connection) ──

    /// A placeholder `QuicTransport` that is never connected.
    ///
    /// Useful for compile-time trait checks and transport selection tests.
    pub struct DisconnectedQuicTransport {
        remote_addr: SocketAddr,
        connected: Arc<AtomicBool>,
    }

    impl DisconnectedQuicTransport {
        pub fn remote_addr(&self) -> SocketAddr {
            self.remote_addr
        }
    }

    #[async_trait]
    impl Transport for DisconnectedQuicTransport {
        async fn send_video(&self, _data: &[u8]) -> ResultType<()> {
            todo!("DisconnectedQuicTransport::send_video")
        }
        async fn recv_video(&self) -> ResultType<Vec<u8>> {
            todo!("DisconnectedQuicTransport::recv_video")
        }
        async fn send_control(&self, _data: &[u8]) -> ResultType<()> {
            todo!("DisconnectedQuicTransport::send_control")
        }
        async fn recv_control(&self) -> ResultType<Vec<u8>> {
            todo!("DisconnectedQuicTransport::recv_control")
        }
        fn is_connected(&self) -> bool {
            self.connected.load(Ordering::Relaxed)
        }
        fn transport_type(&self) -> &str {
            "quic"
        }
    }

    // ── QuicServer ──────────────────────────────────────────────────────

    /// A QUIC server that accepts incoming connections.
    ///
    /// Used on the RustDesk server side to listen for QUIC connections
    /// from clients.
    pub struct QuicServer {
        endpoint: quinn::Endpoint,
    }

    impl QuicServer {
        /// Bind a QUIC server to the given UDP address.
        ///
        /// `cert_der` and `key_der` are the DER-encoded certificate and
        /// private key for TLS. For testing, use `generate_self_signed_cert()`.
        pub fn bind(
            addr: SocketAddr,
            cert_der: Vec<u8>,
            key_der: Vec<u8>,
        ) -> ResultType<Self> {
            let cert = CertificateDer::from(cert_der);
            let key = PrivatePkcs8KeyDer::from(key_der);
            let mut server_config =
                quinn::ServerConfig::with_single_cert(vec![cert], key.into())?;
            server_config.transport_config(Arc::new(build_transport_config(false)));
            let endpoint = quinn::Endpoint::server(server_config, addr)?;
            Ok(Self { endpoint })
        }

        /// Accept the next incoming QUIC connection.
        ///
        /// Returns a `QuicTransport` wrapping the accepted connection, or
        /// `None` if the endpoint has been closed.
        pub async fn accept(&self) -> ResultType<Option<QuicTransport>> {
            match self.endpoint.accept().await {
                Some(incoming) => {
                    let connection = incoming.await?;
                    let transport =
                        QuicTransport::from_connection(self.endpoint.clone(), connection)
                            .await?;
                    Ok(Some(transport))
                }
                None => Ok(None),
            }
        }

        /// Returns the local address the server is bound to.
        pub fn local_addr(&self) -> ResultType<SocketAddr> {
            Ok(self.endpoint.local_addr()?)
        }

        /// Shut down the server, rejecting new connections.
        pub fn close(&self) {
            self.endpoint
                .close(quinn::VarInt::from_u32(0), b"server shutdown");
        }
    }

    // ── Transport config helper ─────────────────────────────────────────

    /// Build a quinn `TransportConfig` with BBR congestion control and
    /// datagrams enabled.
    ///
    /// `is_client`: if true, enables keep-alive interval.
    fn build_transport_config(is_client: bool) -> quinn::TransportConfig {
        let mut transport_config = quinn::TransportConfig::default();
        transport_config.congestion_controller_factory(Arc::new(
            quinn::congestion::BbrConfig::default(),
        ));
        transport_config.datagram_receive_buffer_size(Some(2 * 1024 * 1024));
        if is_client {
            transport_config.keep_alive_interval(Some(Duration::from_secs(5)));
        }
        transport_config
            .max_idle_timeout(Some(Duration::from_secs(30).try_into().unwrap()));
        transport_config
    }

    /// Generate a self-signed certificate and private key for testing.
    ///
    /// Returns `(cert_der, key_der)`.
    pub fn generate_self_signed_cert() -> ResultType<(Vec<u8>, Vec<u8>)> {
        let cert = rcgen::generate_simple_self_signed(vec!["rustdesk".to_string()])?;
        let cert_der = cert.cert.der().to_vec();
        let key_der = cert.key_pair.serialize_der();
        Ok((cert_der, key_der))
    }
}

// ──────────────────────────────────────────────────────────────────────
// Stub implementation (no quic-transport feature)
// ──────────────────────────────────────────────────────────────────────
#[cfg(not(feature = "quic-transport"))]
mod inner {
    use super::*;

    /// A QUIC-based transport for RustDesk peer connections (stub).
    ///
    /// This is a placeholder that compiles without the `quinn` dependency.
    /// All send/recv methods will panic with `todo!()`.
    pub struct QuicTransport {
        remote_addr: SocketAddr,
        connected: Arc<AtomicBool>,
    }

    impl QuicTransport {
        pub async fn connect(_addr: SocketAddr, _server_cert: &[u8]) -> ResultType<Self> {
            todo!("QuicTransport::connect - enable feature 'quic-transport'")
        }

        pub fn new_disconnected(addr: SocketAddr) -> Self {
            Self {
                remote_addr: addr,
                connected: Arc::new(AtomicBool::new(false)),
            }
        }

        pub async fn send_video_frame(&self, _frame: &[u8]) -> ResultType<()> {
            todo!("QuicTransport::send_video_frame - enable feature 'quic-transport'")
        }

        pub async fn recv_video_frame(&self) -> ResultType<Vec<u8>> {
            todo!("QuicTransport::recv_video_frame - enable feature 'quic-transport'")
        }

        pub fn remote_addr(&self) -> SocketAddr {
            self.remote_addr
        }

        pub fn max_datagram_size(&self) -> Option<usize> {
            Some(1200)
        }

        pub fn close(&self, _reason: &str) {
            self.connected.store(false, Ordering::Relaxed);
        }
    }

    #[async_trait]
    impl Transport for QuicTransport {
        async fn send_video(&self, _data: &[u8]) -> ResultType<()> {
            todo!("QuicTransport::send_video - enable feature 'quic-transport'")
        }
        async fn recv_video(&self) -> ResultType<Vec<u8>> {
            todo!("QuicTransport::recv_video - enable feature 'quic-transport'")
        }
        async fn send_control(&self, _data: &[u8]) -> ResultType<()> {
            todo!("QuicTransport::send_control - enable feature 'quic-transport'")
        }
        async fn recv_control(&self) -> ResultType<Vec<u8>> {
            todo!("QuicTransport::recv_control - enable feature 'quic-transport'")
        }
        fn is_connected(&self) -> bool {
            self.connected.load(Ordering::Relaxed)
        }
        fn transport_type(&self) -> &str {
            "quic"
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Re-exports
// ──────────────────────────────────────────────────────────────────────

#[cfg(feature = "quic-transport")]
pub use inner::{
    generate_self_signed_cert, DisconnectedQuicTransport, QuicServer, QuicTransport,
};

#[cfg(not(feature = "quic-transport"))]
pub use inner::QuicTransport;

// ──────────────────────────────────────────────────────────────────────
// Tests (feature-gated)
// ──────────────────────────────────────────────────────────────────────

// ──────────────────────────────────────────────────────────────────────
// Tests: cert caching (always available, no feature gate)
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod cert_tests {
    use super::*;

    #[test]
    fn get_or_generate_quic_cert_returns_consistent_value() {
        // Two calls should return the same cert (cached).
        let (cert1, key1) = get_or_generate_quic_cert();
        let (cert2, key2) = get_or_generate_quic_cert();
        if cfg!(feature = "quic-transport") {
            // With the feature, we get real certs that must be identical.
            assert_eq!(cert1, cert2, "cert should be cached across calls");
            assert_eq!(key1, key2, "key should be cached across calls");
            assert!(!cert1.is_empty(), "cert_der should not be empty");
            assert!(!key1.is_empty(), "key_der should not be empty");
        } else {
            // Without the feature, both should be empty.
            assert!(cert1.is_empty());
            assert!(key1.is_empty());
        }
    }

    #[test]
    fn get_quic_cert_der_returns_cert_only() {
        let cert_der = get_quic_cert_der();
        let (full_cert, _key) = get_or_generate_quic_cert();
        assert_eq!(cert_der, full_cert, "get_quic_cert_der should return cert_der from get_or_generate_quic_cert");
    }

    #[cfg(feature = "quic-transport")]
    #[test]
    fn generated_cert_is_valid_der() {
        let cert_der = get_quic_cert_der();
        // A valid DER-encoded X.509 cert starts with a SEQUENCE tag (0x30).
        assert!(!cert_der.is_empty(), "cert should not be empty");
        assert_eq!(
            cert_der[0], 0x30,
            "DER cert should start with SEQUENCE tag (0x30), got 0x{:02x}",
            cert_der[0]
        );
    }

    #[cfg(not(feature = "quic-transport"))]
    #[test]
    fn stub_cert_is_empty() {
        let cert_der = get_quic_cert_der();
        assert!(cert_der.is_empty(), "stub should return empty cert");
    }

    #[test]
    fn get_or_generate_quic_cert_is_thread_safe() {
        // Verify that concurrent calls don't panic or produce inconsistent results.
        use std::thread;
        let handles: Vec<_> = (0..4)
            .map(|_| {
                thread::spawn(|| get_or_generate_quic_cert())
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        // All results should be identical.
        for r in &results[1..] {
            assert_eq!(r.0, results[0].0, "all threads should get the same cert");
            assert_eq!(r.1, results[0].1, "all threads should get the same key");
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Tests: QUIC transport (feature-gated)
// ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[cfg(feature = "quic-transport")]
mod tests {
    use super::*;
    use hbb_common::tokio;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cached_cert_works_with_quic_connect() {
        // Verify that the cached cert from get_or_generate_quic_cert() can
        // be used to establish a QUIC connection (loopback test).
        let (cert_der, key_der) = get_or_generate_quic_cert();
        assert!(!cert_der.is_empty());
        assert!(!key_der.is_empty());

        let server =
            QuicServer::bind("127.0.0.1:0".parse().unwrap(), cert_der.clone(), key_der)
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
        let server_t = server_result.unwrap();
        let client_t = client_result.unwrap();

        assert!(server_t.is_connected());
        assert!(client_t.is_connected());

        // Send a message to verify the connection works.
        client_t.send_control(b"hello").await.unwrap();
        let received = server_t.recv_control().await.unwrap();
        assert_eq!(received, b"hello");
    }

    #[tokio::test]
    async fn empty_cert_fails_gracefully() {
        // An empty certificate should cause QuicTransport::connect to fail,
        // which triggers TCP fallback in try_quic_connection.
        let result = QuicTransport::connect(
            "127.0.0.1:19999".parse().unwrap(),
            &[],
        )
        .await;
        assert!(result.is_err(), "empty cert should fail");
    }

    /// Helper: spin up a QuicServer and connect a QuicTransport client to it.
    /// Returns `(server_transport, client_transport)`.
    async fn loopback_pair() -> (QuicTransport, QuicTransport) {
        let (cert_der, key_der) = generate_self_signed_cert().unwrap();

        let server =
            QuicServer::bind("127.0.0.1:0".parse().unwrap(), cert_der.clone(), key_der)
                .unwrap();
        let server_addr = server.local_addr().unwrap();

        // Spawn both server accept and client connect concurrently
        // to avoid deadlock (both need the QUIC handshake to complete)
        let server_handle = tokio::spawn(async move {
            server.accept().await.unwrap().unwrap()
        });

        let client_handle = tokio::spawn({
            let cert = cert_der.clone();
            async move { QuicTransport::connect(server_addr, &cert).await.unwrap() }
        });

        let (server_result, client_result) = tokio::join!(server_handle, client_handle);
        (server_result.unwrap(), client_result.unwrap())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn loopback_control_message_roundtrip() {
        let (server_t, client_t) = loopback_pair().await;

        // Client sends control message, server receives it.
        let msg = b"hello from client";
        client_t.send_control(msg).await.unwrap();
        let received = server_t.recv_control().await.unwrap();
        assert_eq!(received, msg);

        // Server sends reply, client receives it.
        let reply = b"hello from server";
        server_t.send_control(reply).await.unwrap();
        let received = client_t.recv_control().await.unwrap();
        assert_eq!(received, reply);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn datagram_video_send_recv() {
        let (server_t, client_t) = loopback_pair().await;

        // Client sends video datagram, server receives it.
        let frame = vec![0xAB; 500];
        client_t.send_video(&frame).await.unwrap();
        let received = server_t.recv_video().await.unwrap();
        assert_eq!(received, frame);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connection_close_and_reconnect() {
        let (cert_der, key_der) = generate_self_signed_cert().unwrap();

        let server = QuicServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            cert_der.clone(),
            key_der.clone(),
        )
        .unwrap();
        let server_addr = server.local_addr().unwrap();

        // First connection — spawn client, accept on main task.
        let client_handle1 = tokio::spawn({
            let cert = cert_der.clone();
            async move { QuicTransport::connect(server_addr, &cert).await.unwrap() }
        });
        let _server_t1 = server.accept().await.unwrap().unwrap();
        let client1 = client_handle1.await.unwrap();

        assert!(client1.is_connected());
        client1.close("test close");
        assert!(!client1.is_connected());

        // Second connection to the same server.
        let client_handle2 = tokio::spawn({
            let cert = cert_der.clone();
            async move { QuicTransport::connect(server_addr, &cert).await.unwrap() }
        });
        let _server_t2 = server.accept().await.unwrap().unwrap();
        let client2 = client_handle2.await.unwrap();

        assert!(client2.is_connected());
        assert_eq!(client2.transport_type(), "quic");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transport_type_reports_quic() {
        let (server_t, client_t) = loopback_pair().await;
        assert_eq!(server_t.transport_type(), "quic");
        assert_eq!(client_t.transport_type(), "quic");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn max_datagram_size_is_some() {
        let (server_t, client_t) = loopback_pair().await;
        // Both sides should support datagrams since we configured them.
        assert!(server_t.max_datagram_size().is_some());
        assert!(client_t.max_datagram_size().is_some());
    }

    #[tokio::test]
    async fn disconnected_transport_is_not_connected() {
        let t = QuicTransport::new_disconnected("127.0.0.1:0".parse().unwrap());
        assert!(!t.is_connected());
        assert_eq!(t.transport_type(), "quic");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multiple_control_messages() {
        let (server_t, client_t) = loopback_pair().await;

        for i in 0..10u32 {
            let msg = format!("message-{}", i);
            client_t.send_control(msg.as_bytes()).await.unwrap();
            let received = server_t.recv_control().await.unwrap();
            assert_eq!(received, msg.as_bytes());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn large_control_message() {
        let (server_t, client_t) = loopback_pair().await;

        // Send a 64KB control message (larger than typical MTU, must go over stream).
        let msg = vec![0x42; 64 * 1024];
        client_t.send_control(&msg).await.unwrap();
        let received = server_t.recv_control().await.unwrap();
        assert_eq!(received, msg);
    }
}
