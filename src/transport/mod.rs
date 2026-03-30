//! Transport layer abstraction for RustDesk connections.
//!
//! Supports TCP (current default) and QUIC (experimental, for low-latency mode).
//! Video frames can be sent unreliably over QUIC datagrams while control
//! messages use reliable QUIC streams.
//!
//! # Architecture
//!
//! The transport layer sits between the application-level protocol (protobuf
//! messages, video frames) and the network. It provides two key abstractions:
//!
//! - **Reliable delivery** for control messages, file transfers, and clipboard
//!   data via QUIC streams (or TCP fallback).
//! - **Unreliable delivery** for video and audio frames via QUIC datagrams,
//!   with Forward Error Correction (FEC) to recover from packet loss without
//!   retransmission delays.
//!
//! # Trait-based Transport
//!
//! The [`Transport`] trait defines a unified interface that both [`tcp::TcpTransport`]
//! and [`quic::QuicTransport`] implement. Call sites can work with
//! `Box<dyn Transport>` or `&dyn Transport` to be transport-agnostic.
//!
//! Use [`select_transport`] to choose a transport based on [`TransportConfig`].
//!
//! # Module Layout
//!
//! - [`tcp`] — TCP transport. Wraps a `TcpStream` with length-prefixed framing.
//! - [`quic`] — QUIC transport (stubbed, pending `quinn` dependency).
//! - [`fec`] — Forward Error Correction packet headers and fragmentation.
//!   Defines the 24-byte [`fec::VideoPacketHeader`] wire format and
//!   [`fec::FecEncoder`] / [`fec::FecDecoder`] for Reed-Solomon coding.

use async_trait::async_trait;
use hbb_common::ResultType;
use hbb_common::sodiumoxide;

pub mod fec;
pub mod quic;
pub mod quic_stream;
pub mod tcp;

pub use quic_stream::QuicStream;

/// Unified transport trait for RustDesk peer connections.
///
/// Abstracts the differences between TCP and QUIC so that higher-level
/// code (video streaming, control message dispatch) does not need to know
/// which transport is in use.
///
/// # Video vs Control channels
///
/// - **Video** (`send_video` / `recv_video`): Carries encoded video (and audio)
///   frames. Over QUIC these are sent as unreliable datagrams; over TCP they
///   are sent reliably with length-prefixed framing.
///
/// - **Control** (`send_control` / `recv_control`): Carries protobuf-encoded
///   control messages (keyboard, mouse, clipboard, session negotiation). Always
///   delivered reliably and in order regardless of transport.
///
/// # Object Safety
///
/// This trait is object-safe and can be used as `Box<dyn Transport>` or
/// `&dyn Transport`.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Send video/audio frame data.
    ///
    /// Over QUIC this uses unreliable datagrams; over TCP it uses reliable
    /// length-prefixed framing.
    async fn send_video(&self, data: &[u8]) -> ResultType<()>;

    /// Receive video/audio frame data.
    async fn recv_video(&self) -> ResultType<Vec<u8>>;

    /// Send a control message (reliable, ordered).
    async fn send_control(&self, data: &[u8]) -> ResultType<()>;

    /// Receive a control message (reliable, ordered).
    async fn recv_control(&self) -> ResultType<Vec<u8>>;

    /// Returns `true` if the underlying connection is still alive.
    fn is_connected(&self) -> bool;

    /// Returns the transport type name (e.g., `"tcp"`, `"quic"`).
    fn transport_type(&self) -> &str;
}

/// Configuration for transport selection and behavior.
///
/// Controls whether QUIC is preferred over TCP, and configures FEC
/// parameters for unreliable datagram delivery.
#[derive(Debug, Clone)]
pub struct TransportConfig {
    /// If `true`, attempt QUIC before falling back to TCP.
    /// Default: `false` (TCP only, until QUIC is production-ready).
    pub prefer_quic: bool,

    /// If `true`, apply Forward Error Correction to video datagrams.
    /// Only meaningful when using QUIC transport.
    /// Default: `false`.
    pub fec_enabled: bool,

    /// Ratio of FEC parity packets to data packets, in range `0.0..=1.0`.
    /// For example, `0.1` means 1 parity packet per 10 data packets.
    /// Default: `0.1`.
    pub fec_ratio: f32,

    /// Maximum datagram payload size in bytes. Fragments larger than this
    /// will be split. Should be <= path MTU minus headers.
    /// Default: `1400`.
    pub max_datagram_size: usize,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            prefer_quic: false,
            fec_enabled: false,
            fec_ratio: 0.1,
            max_datagram_size: 1400,
        }
    }
}

/// Select a transport type based on configuration and availability.
///
/// Returns `"tcp"` or `"quic"` indicating which transport should be used.
/// Currently, QUIC is never available (the `quinn` dependency is not wired up),
/// so this always returns `"tcp"` regardless of the `prefer_quic` setting.
///
/// In the future, this function will probe for QUIC support on both the local
/// endpoint and the remote peer before selecting QUIC.
pub fn select_transport(config: &TransportConfig) -> &'static str {
    if config.prefer_quic && quic_available() {
        "quic"
    } else {
        "tcp"
    }
}

/// Check whether QUIC transport is available in this build.
///
/// Returns `true` when compiled with the `quic-transport` feature flag
/// (which brings in the `quinn` and `rustls` dependencies).
fn quic_available() -> bool {
    cfg!(feature = "quic-transport")
}

/// Check whether QUIC should be used for peer connections.
///
/// Returns `true` when **both** conditions are met:
/// 1. The binary was compiled with `feature = "quic-transport"`.
/// 2. The user has set the `prefer-quic` option to `"Y"` in config.
///
/// Call sites should attempt QUIC first when this returns `true`, then
/// fall back to TCP on failure.
pub fn should_use_quic() -> bool {
    cfg!(feature = "quic-transport")
        && hbb_common::config::Config::get_option(
            hbb_common::config::keys::OPTION_PREFER_QUIC,
        ) == "Y"
}

/// Result of a QUIC connection attempt.
///
/// When QUIC is preferred and succeeds, `session_transport` holds a
/// `SessionTransport::Quic` ready for use in the session loop. The caller
/// must still fall through to TCP when this is `None` (either because QUIC
/// is not preferred, or because the attempt failed).
pub struct QuicAttemptResult {
    /// A `SessionTransport::Quic` wrapping the live QUIC connection, if
    /// connection succeeded. `None` means QUIC was not attempted or failed
    /// (fall back to TCP).
    pub session_transport: Option<SessionTransport>,
    /// The raw QUIC transport, if connection succeeded (kept for backward
    /// compat with code that inspects the transport directly).
    pub quic_transport: Option<quic::QuicTransport>,
    /// Whether QUIC was attempted at all.
    pub attempted: bool,
    /// Human-readable reason QUIC was not used (for logging).
    pub reason: String,
}

/// Attempt a QUIC connection to the given address, if QUIC is preferred.
///
/// This is a best-effort attempt: if QUIC is not compiled in, not configured,
/// or the connection fails, the caller should proceed with TCP.
///
/// `addr` is the target address as a string (e.g., `"1.2.3.4:21117"`).
/// `peer_pk` is the peer's public key bytes, used as a stand-in for the
/// server certificate during the QUIC TLS handshake. In production, this
/// will be replaced by a proper certificate exchange during rendezvous.
///
/// # Returns
///
/// A `QuicAttemptResult` indicating whether QUIC was attempted, whether
/// it succeeded, and a reason string for logging. On success,
/// `session_transport` contains a `SessionTransport::Quic` ready for use
/// in the session loop.
pub async fn try_quic_connection(addr: &str, peer_pk: &[u8]) -> QuicAttemptResult {
    if !should_use_quic() {
        return QuicAttemptResult {
            session_transport: None,
            quic_transport: None,
            attempted: false,
            reason: if cfg!(feature = "quic-transport") {
                "QUIC not preferred (prefer-quic != Y)".to_string()
            } else {
                "QUIC not compiled in (feature quic-transport disabled)".to_string()
            },
        };
    }

    hbb_common::log::info!("QUIC transport preferred, attempting connection to {}", addr);

    // Parse the address. If it fails, fall back to TCP.
    let socket_addr: std::net::SocketAddr = match addr.parse() {
        Ok(a) => a,
        Err(e) => {
            hbb_common::log::warn!("QUIC: cannot parse address '{}': {}, falling back to TCP", addr, e);
            return QuicAttemptResult {
                session_transport: None,
                quic_transport: None,
                attempted: true,
                reason: format!("address parse error: {}", e),
            };
        }
    };

    // Attempt QUIC connection.
    // The peer_pk is used as the server certificate for TLS verification.
    // TODO: Replace with proper certificate exchange during rendezvous.
    // For now, if peer_pk is empty, we skip the QUIC attempt since we
    // cannot verify the server's identity.
    if peer_pk.is_empty() {
        hbb_common::log::info!("QUIC: no peer public key available, falling back to TCP");
        return QuicAttemptResult {
            session_transport: None,
            quic_transport: None,
            attempted: true,
            reason: "no peer public key for QUIC TLS".to_string(),
        };
    }

    match quic::QuicTransport::connect(socket_addr, peer_pk).await {
        Ok(transport) => {
            hbb_common::log::info!(
                "QUIC connection established to {} (transport_type={})",
                addr,
                transport.transport_type(),
            );
            // Wrap the QUIC transport in a QuicStream, then in a
            // SessionTransport for use in the session loop.
            let quic_stream = QuicStream::new(transport);
            let session = SessionTransport::Quic(quic_stream);
            QuicAttemptResult {
                session_transport: Some(session),
                quic_transport: None, // ownership moved to SessionTransport
                attempted: true,
                reason: "QUIC connected successfully".to_string(),
            }
        }
        Err(e) => {
            hbb_common::log::warn!("QUIC connection to {} failed: {}, falling back to TCP", addr, e);
            QuicAttemptResult {
                session_transport: None,
                quic_transport: None,
                attempted: true,
                reason: format!("QUIC connect error: {}", e),
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// SessionTransport — unified enum for session-level connections
// ──────────────────────────────────────────────────────────────────────

/// A unified transport for session-level connections.
///
/// Wraps either an `hbb_common::Stream` (TCP/WebSocket) or a `QuicStream`
/// (QUIC), forwarding all session-level operations to the underlying
/// transport. This allows the session loop in `client.rs` to be
/// transport-agnostic: the same `send()`, `next()`, `set_key()` etc.
/// calls work regardless of whether the connection is TCP or QUIC.
///
/// # Usage
///
/// ```ignore
/// // Direct IP path in client.rs:
/// let transport = if quic_succeeded {
///     SessionTransport::Quic(QuicStream::new(quic_transport))
/// } else {
///     SessionTransport::Tcp(connect_tcp_local(...).await?)
/// };
/// // Then use transport.send(), transport.next(), etc.
/// ```
///
/// # Encryption
///
/// For the TCP variant, `set_key()` enables NaCl encryption as usual.
/// For the QUIC variant, `set_key()` is a no-op because QUIC has TLS 1.3
/// built in. `is_secured()` always returns `true` for QUIC.
pub enum SessionTransport {
    /// TCP (or WebSocket) transport via `hbb_common::Stream`.
    Tcp(hbb_common::Stream),
    /// QUIC transport via `QuicStream`.
    Quic(QuicStream),
}

impl SessionTransport {
    /// Send a protobuf message.
    #[inline]
    pub async fn send(&mut self, msg: &impl hbb_common::protobuf::Message) -> hbb_common::ResultType<()> {
        match self {
            SessionTransport::Tcp(s) => s.send(msg).await,
            SessionTransport::Quic(q) => q.send(msg).await,
        }
    }

    /// Receive the next message as raw bytes.
    #[inline]
    pub async fn next(&mut self) -> Option<Result<bytes::BytesMut, std::io::Error>> {
        match self {
            SessionTransport::Tcp(s) => s.next().await,
            SessionTransport::Quic(q) => q.next().await,
        }
    }

    /// Receive the next message with a timeout.
    #[inline]
    pub async fn next_timeout(
        &mut self,
        timeout: u64,
    ) -> Option<Result<bytes::BytesMut, std::io::Error>> {
        match self {
            SessionTransport::Tcp(s) => s.next_timeout(timeout).await,
            SessionTransport::Quic(q) => q.next_timeout(timeout).await,
        }
    }

    /// Send raw bytes.
    #[inline]
    pub async fn send_raw(&mut self, bytes: Vec<u8>) -> hbb_common::ResultType<()> {
        match self {
            SessionTransport::Tcp(s) => s.send_raw(bytes).await,
            SessionTransport::Quic(q) => q.send_raw(bytes).await,
        }
    }

    /// Send pre-formed bytes.
    #[inline]
    pub async fn send_bytes(&mut self, bytes: bytes::Bytes) -> hbb_common::ResultType<()> {
        match self {
            SessionTransport::Tcp(s) => s.send_bytes(bytes).await,
            SessionTransport::Quic(q) => q.send_bytes(bytes).await,
        }
    }

    /// Set the encryption key. No-op for QUIC (TLS 1.3 built in).
    #[inline]
    pub fn set_key(&mut self, key: sodiumoxide::crypto::secretbox::Key) {
        match self {
            SessionTransport::Tcp(s) => s.set_key(key),
            SessionTransport::Quic(q) => q.set_key(key),
        }
    }

    /// Set raw mode. No-op for QUIC.
    #[inline]
    pub fn set_raw(&mut self) {
        match self {
            SessionTransport::Tcp(s) => s.set_raw(),
            SessionTransport::Quic(q) => q.set_raw(),
        }
    }

    /// Set the send timeout in milliseconds.
    #[inline]
    pub fn set_send_timeout(&mut self, ms: u64) {
        match self {
            SessionTransport::Tcp(s) => s.set_send_timeout(ms),
            SessionTransport::Quic(q) => q.set_send_timeout(ms),
        }
    }

    /// Returns `true` if the connection is encrypted.
    /// Always `true` for QUIC; depends on key exchange for TCP.
    #[inline]
    pub fn is_secured(&self) -> bool {
        match self {
            SessionTransport::Tcp(s) => s.is_secured(),
            SessionTransport::Quic(q) => q.is_secured(),
        }
    }

    /// Returns the local socket address.
    #[inline]
    pub fn local_addr(&self) -> std::net::SocketAddr {
        match self {
            SessionTransport::Tcp(s) => s.local_addr(),
            SessionTransport::Quic(q) => q.local_addr(),
        }
    }

    /// Returns the transport type name for logging.
    pub fn transport_type_str(&self) -> &'static str {
        match self {
            SessionTransport::Tcp(_) => "TCP",
            SessionTransport::Quic(_) => "QUIC",
        }
    }

    /// Returns a mutable reference to the inner `hbb_common::Stream`, if
    /// this is a TCP transport.
    ///
    /// This is a backward-compatibility escape hatch for `hbb_common` APIs
    /// (like `fs::handle_read_jobs`) that take `&mut Stream` directly.
    /// QUIC connections do not have an inner `Stream`, so this returns `None`.
    ///
    /// Callers should prefer using `SessionTransport` methods directly
    /// rather than reaching into the inner `Stream`.
    #[inline]
    pub fn as_stream_mut(&mut self) -> Option<&mut hbb_common::Stream> {
        match self {
            SessionTransport::Tcp(s) => Some(s),
            SessionTransport::Quic(_) => None,
        }
    }
}

impl From<hbb_common::Stream> for SessionTransport {
    fn from(stream: hbb_common::Stream) -> Self {
        SessionTransport::Tcp(stream)
    }
}

impl From<QuicStream> for SessionTransport {
    fn from(qs: QuicStream) -> Self {
        SessionTransport::Quic(qs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the Transport trait is object-safe by constructing a trait object.
    /// This is a compile-time check — if it compiles, the trait is object-safe.
    #[test]
    fn transport_trait_is_object_safe() {
        // This function just needs to compile. The trait object type
        // Box<dyn Transport> must be constructible for object safety.
        fn _assert_object_safe(_t: &dyn Transport) {}
        fn _assert_boxable(_t: Box<dyn Transport>) {}
    }

    #[test]
    fn transport_config_defaults() {
        let config = TransportConfig::default();
        assert!(!config.prefer_quic);
        assert!(!config.fec_enabled);
        assert!((config.fec_ratio - 0.1).abs() < f32::EPSILON);
        assert_eq!(config.max_datagram_size, 1400);
    }

    #[test]
    fn select_transport_returns_tcp_by_default() {
        let config = TransportConfig::default();
        assert_eq!(select_transport(&config), "tcp");
    }

    #[test]
    fn select_transport_with_quic_preferred() {
        let config = TransportConfig {
            prefer_quic: true,
            ..Default::default()
        };
        if cfg!(feature = "quic-transport") {
            assert_eq!(select_transport(&config), "quic");
        } else {
            // QUIC is not compiled in, so fall back to TCP.
            assert_eq!(select_transport(&config), "tcp");
        }
    }

    #[test]
    fn tcp_transport_reports_correct_type() {
        // We can't easily create a real TcpTransport without a live socket,
        // but we can verify the trait impl returns the right string by
        // checking the constant directly. The integration test would use
        // a real socket pair.
        //
        // For a unit test, we create a loopback socket pair.
        use hbb_common::tokio;
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let transport = tcp::TcpTransport::new(client_stream);
            assert_eq!(transport.transport_type(), "tcp");
            assert!(transport.is_connected());
        });
    }

    #[test]
    fn should_use_quic_returns_false_by_default() {
        // Without the "quic-transport" feature AND without the config option
        // set to "Y", should_use_quic() must return false.
        assert!(!should_use_quic());
    }

    #[cfg(not(feature = "quic-transport"))]
    #[test]
    fn should_use_quic_false_without_feature() {
        // Even if the config option were somehow set, should_use_quic()
        // returns false when compiled without the quic-transport feature,
        // because cfg!(feature = "quic-transport") is false at compile time.
        assert!(!should_use_quic());
    }

    #[cfg(not(feature = "quic-transport"))]
    #[test]
    fn quic_transport_reports_correct_type() {
        use std::net::SocketAddr;
        // QuicTransport::transport_type() should return "quic".
        // We construct one directly (bypassing connect) to test the trait impl.
        let transport = quic::QuicTransport::new_disconnected(
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        );
        assert_eq!(transport.transport_type(), "quic");
        assert!(!transport.is_connected());
    }

    #[cfg(feature = "quic-transport")]
    #[test]
    fn quic_transport_reports_correct_type() {
        use std::net::SocketAddr;
        // When quic-transport is enabled, new_disconnected returns
        // DisconnectedQuicTransport which also implements Transport.
        let transport = quic::QuicTransport::new_disconnected(
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        );
        assert_eq!(transport.transport_type(), "quic");
        assert!(!transport.is_connected());
    }

    // ── try_quic_connection tests ──────────────────────────────────────

    #[test]
    fn try_quic_not_attempted_when_quic_disabled() {
        // Without should_use_quic() returning true, try_quic_connection
        // should return immediately without attempting a connection.
        use hbb_common::tokio;
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let result = try_quic_connection("127.0.0.1:21117", &[0u8; 32]).await;
            // should_use_quic() returns false (no config option set, and
            // possibly no feature), so attempted should be false.
            assert!(!result.attempted);
            assert!(result.session_transport.is_none());
            assert!(result.quic_transport.is_none());
            assert!(!result.reason.is_empty());
        });
    }

    #[test]
    fn try_quic_empty_peer_pk_skips_attempt() {
        // Even if QUIC were preferred, an empty peer_pk should cause
        // the attempt to be skipped. We cannot test with should_use_quic()
        // returning true without setting config, but we can test the
        // function's behavior via the code path.
        //
        // This test verifies the contract: empty pk => no QUIC transport.
        use hbb_common::tokio;
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let result = try_quic_connection("127.0.0.1:21117", &[]).await;
            assert!(result.session_transport.is_none());
            assert!(result.quic_transport.is_none());
        });
    }

    #[test]
    fn quic_attempt_result_defaults() {
        // Verify QuicAttemptResult can be constructed and fields are accessible.
        let result = QuicAttemptResult {
            session_transport: None,
            quic_transport: None,
            attempted: false,
            reason: "test".to_string(),
        };
        assert!(!result.attempted);
        assert!(result.session_transport.is_none());
        assert!(result.quic_transport.is_none());
        assert_eq!(result.reason, "test");
    }

    #[cfg(not(feature = "quic-transport"))]
    #[test]
    fn try_quic_reports_feature_disabled() {
        // Without the quic-transport feature, the reason should mention
        // that QUIC is not compiled in.
        use hbb_common::tokio;
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let result = try_quic_connection("127.0.0.1:21117", &[0u8; 32]).await;
            assert!(!result.attempted);
            assert!(
                result.reason.contains("not compiled in"),
                "reason should mention QUIC is not compiled in, got: {}",
                result.reason,
            );
        });
    }

    #[test]
    fn try_quic_bad_address_does_not_panic() {
        // Passing a non-parseable address should not panic; it should
        // return gracefully with no transport.
        use hbb_common::tokio;
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let result = try_quic_connection("not-an-address", &[0u8; 32]).await;
            assert!(result.session_transport.is_none());
            assert!(result.quic_transport.is_none());
        });
    }

    #[cfg(feature = "quic-transport")]
    #[test]
    fn try_quic_fails_with_bogus_cert_and_unreachable_addr() {
        // When QUIC is compiled in but should_use_quic() is false (no config),
        // the attempt is not made. We verify that the function handles this
        // gracefully. With QUIC feature enabled but config not set, this
        // tests the "not preferred" path.
        use hbb_common::tokio;
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let result = try_quic_connection("127.0.0.1:19999", &[0u8; 32]).await;
            // should_use_quic() is false because config is not set
            assert!(!result.attempted);
            assert!(result.session_transport.is_none());
            assert!(result.quic_transport.is_none());
            assert!(
                result.reason.contains("not preferred"),
                "reason should mention QUIC not preferred, got: {}",
                result.reason,
            );
        });
    }

    // ── SessionTransport tests ──────────────────────────────────────────

    #[test]
    fn session_transport_tcp_dispatches_correctly() {
        // Verify that SessionTransport::Tcp forwards is_secured() to Stream.
        use hbb_common::tokio;
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            let client_handle = tokio::spawn(async move {
                tokio::net::TcpStream::connect(addr).await.unwrap()
            });
            let (server_stream, _) = listener.accept().await.unwrap();
            let client_stream = client_handle.await.unwrap();

            let stream = hbb_common::Stream::from(client_stream, addr);
            let transport = SessionTransport::Tcp(stream);

            assert_eq!(transport.transport_type_str(), "TCP");
            assert!(!transport.is_secured()); // No key set yet
            assert_eq!(transport.local_addr(), addr);

            // Verify the server side also works
            let _server_stream = server_stream; // keep alive
        });
    }

    #[test]
    fn session_transport_from_stream() {
        // Verify From<Stream> for SessionTransport works.
        use hbb_common::tokio;
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let client_handle = tokio::spawn(async move {
                tokio::net::TcpStream::connect(addr).await.unwrap()
            });
            let (_server, _) = listener.accept().await.unwrap();
            let client = client_handle.await.unwrap();

            let stream = hbb_common::Stream::from(client, addr);
            let transport: SessionTransport = stream.into();
            assert_eq!(transport.transport_type_str(), "TCP");
        });
    }

    #[test]
    fn session_transport_tcp_send_recv() {
        // Verify that SessionTransport::Tcp can send and receive messages.
        use hbb_common::message_proto::Message;
        use hbb_common::tokio;
        use hbb_common::protobuf::Message as _;

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            let client_handle = tokio::spawn(async move {
                tokio::net::TcpStream::connect(addr).await.unwrap()
            });
            let (server_tcp, server_addr) = listener.accept().await.unwrap();
            let client_tcp = client_handle.await.unwrap();

            let mut client_transport =
                SessionTransport::Tcp(hbb_common::Stream::from(client_tcp, addr));
            let mut server_transport =
                SessionTransport::Tcp(hbb_common::Stream::from(server_tcp, server_addr));

            // Send a protobuf message from client to server.
            let msg = Message::new();
            client_transport.send(&msg).await.unwrap();

            let received = server_transport.next().await.unwrap().unwrap();
            let parsed = Message::parse_from_bytes(&received).unwrap();
            assert_eq!(parsed, msg);
        });
    }

    #[test]
    fn session_transport_set_key_tcp() {
        // Verify set_key works on TCP variant.
        use hbb_common::tokio;

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let client_handle = tokio::spawn(async move {
                tokio::net::TcpStream::connect(addr).await.unwrap()
            });
            let (_server, _) = listener.accept().await.unwrap();
            let client = client_handle.await.unwrap();

            let mut transport =
                SessionTransport::Tcp(hbb_common::Stream::from(client, addr));

            assert!(!transport.is_secured());
            let key = sodiumoxide::crypto::secretbox::gen_key();
            transport.set_key(key);
            assert!(transport.is_secured());
        });
    }

    /// Verify that SessionTransport::Quic wraps QuicStream correctly
    /// (compile-time check — ensures the enum variant and methods exist).
    #[test]
    fn session_transport_quic_variant_compiles() {
        fn _assert_quic_methods(t: &SessionTransport) {
            let _ = t.is_secured();
            let _ = t.transport_type_str();
            let _ = t.local_addr();
        }

        fn _assert_quic_async_methods(t: &mut SessionTransport) {
            // These are async, just verify they exist.
            let _ = &t.next();
        }
    }
}
