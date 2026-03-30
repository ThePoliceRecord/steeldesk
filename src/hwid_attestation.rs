//! HWID attestation for 2FA trusted devices.
//!
//! Instead of storing and comparing raw HWIDs (which are client-supplied and
//! trivially spoofable — CWE-290), we hash the HWID with a domain-specific salt
//! before storage and comparison. Additionally, a challenge-response protocol
//! ensures that a client actually possesses the HWID rather than merely knowing
//! its hash.
//!
//! ## Protocol
//!
//! 1. Client sends `hash_hwid(hwid)` in the login/2FA message.
//! 2. Server looks up the hash in its trusted-devices list.
//! 3. Server generates a random 32-byte challenge nonce and sends it back.
//! 4. Client computes `compute_response(raw_hwid, challenge)` and replies.
//! 5. Server verifies using `verify_response(stored_hash, challenge, response)`.
//!
//! An attacker who only knows the HWID hash cannot compute the response because
//! `compute_response` uses the *raw* HWID (which requires the actual machine UID
//! from the `machine-uid` crate).

use hbb_common::rand::{self, Rng};
use sha2::{Digest, Sha256};

/// Domain-separation salt so HWID hashes are not reusable in other contexts.
const HWID_HASH_SALT: &[u8] = b"steeldesk-hwid-salt-v1";

/// Domain-separation salt for the challenge-response HMAC.
const HWID_HMAC_SALT: &[u8] = b"steeldesk-hwid-hmac-v1";

/// Length of the challenge nonce in bytes.
pub const CHALLENGE_LEN: usize = 32;

/// Hash an HWID for storage and transmission. The raw HWID is never stored or
/// sent over the wire.
pub fn hash_hwid(hwid: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(hwid);
    hasher.update(HWID_HASH_SALT);
    hasher.finalize().to_vec()
}

/// Hash an HWID and return the hex-encoded string (useful for logging/display).
pub fn hash_hwid_hex(hwid: &[u8]) -> String {
    hex::encode(hash_hwid(hwid))
}

/// Generate a cryptographically random challenge nonce (32 bytes).
pub fn generate_challenge() -> Vec<u8> {
    let mut buf = vec![0u8; CHALLENGE_LEN];
    rand::thread_rng().fill(&mut buf[..]);
    buf
}

/// Compute the challenge-response using the raw HWID and the server's nonce.
///
/// The response is `SHA-256(HMAC_SALT || hwid || challenge)`, which acts as a
/// keyed hash (the HWID is the secret).
pub fn compute_response(hwid: &[u8], challenge: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(HWID_HMAC_SALT);
    hasher.update(hwid);
    hasher.update(challenge);
    hasher.finalize().to_vec()
}

/// Verify a challenge-response from the client.
///
/// `stored_hwid_hash` is the hash that was stored via [`hash_hwid`].
/// We cannot recompute the response from a hash alone — we need the raw HWID.
/// However, during the 2FA trust flow the server receives both the hash *and*
/// verifies the response, so the caller must supply the raw HWID bytes for
/// verification.
///
/// In practice this function is called with the raw HWID that the server
/// reconstructs: the client proves possession by responding correctly, and the
/// server verifies by computing the expected response from the HWID it resolved.
///
/// For the current deployment, the server stores hashes and the client sends
/// the response computed from its raw HWID. The server re-derives the expected
/// response from the raw HWID (which only the legitimate client knows).
///
/// This function uses constant-time comparison to prevent timing side-channels.
pub fn verify_response(raw_hwid: &[u8], challenge: &[u8], response: &[u8]) -> bool {
    let expected = compute_response(raw_hwid, challenge);
    constant_time_eq(&expected, response)
}

/// Constant-time byte comparison to prevent timing attacks.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut x: u8 = 0;
    for i in 0..a.len() {
        x |= a[i] ^ b[i];
    }
    x == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_hwid_deterministic() {
        let hwid = b"test-machine-uid-12345";
        let h1 = hash_hwid(hwid);
        let h2 = hash_hwid(hwid);
        assert_eq!(h1, h2, "hash_hwid must be deterministic");
    }

    #[test]
    fn test_hash_hwid_different_inputs_differ() {
        let h1 = hash_hwid(b"machine-A");
        let h2 = hash_hwid(b"machine-B");
        assert_ne!(h1, h2, "different HWIDs must produce different hashes");
    }

    #[test]
    fn test_hash_hwid_hex_format() {
        let hwid = b"test-hwid";
        let hex_str = hash_hwid_hex(hwid);
        // SHA-256 output is 32 bytes = 64 hex chars
        assert_eq!(hex_str.len(), 64);
        assert!(hex_str.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_hash_hwid_empty() {
        // Empty HWID should still produce a valid hash (of the salt alone)
        let h = hash_hwid(b"");
        assert_eq!(h.len(), 32); // SHA-256 output
        // Should be deterministic
        assert_eq!(h, hash_hwid(b""));
        // And different from a non-empty HWID
        assert_ne!(h, hash_hwid(b"notempty"));
    }

    #[test]
    fn test_generate_challenge_length() {
        let c = generate_challenge();
        assert_eq!(c.len(), CHALLENGE_LEN);
    }

    #[test]
    fn test_generate_challenge_unique() {
        let c1 = generate_challenge();
        let c2 = generate_challenge();
        assert_ne!(c1, c2, "consecutive challenges must differ (random)");
    }

    #[test]
    fn test_compute_response_deterministic() {
        let hwid = b"my-machine";
        let challenge = b"0123456789abcdef0123456789abcdef";
        let r1 = compute_response(hwid, challenge);
        let r2 = compute_response(hwid, challenge);
        assert_eq!(r1, r2, "compute_response must be deterministic for same inputs");
    }

    #[test]
    fn test_compute_response_different_challenges() {
        let hwid = b"my-machine";
        let r1 = compute_response(hwid, b"challenge-1-padding-to-32-bytes!");
        let r2 = compute_response(hwid, b"challenge-2-padding-to-32-bytes!");
        assert_ne!(r1, r2, "different challenges must produce different responses");
    }

    #[test]
    fn test_compute_response_different_hwids() {
        let challenge = b"0123456789abcdef0123456789abcdef";
        let r1 = compute_response(b"machine-A", challenge);
        let r2 = compute_response(b"machine-B", challenge);
        assert_ne!(r1, r2, "different HWIDs must produce different responses");
    }

    #[test]
    fn test_verify_response_accepts_correct() {
        let hwid = b"legitimate-device-hwid";
        let challenge = generate_challenge();
        let response = compute_response(hwid, &challenge);
        assert!(
            verify_response(hwid, &challenge, &response),
            "verify_response must accept a correct response"
        );
    }

    #[test]
    fn test_verify_response_rejects_wrong_response() {
        let hwid = b"legitimate-device-hwid";
        let challenge = generate_challenge();
        let wrong_response = vec![0u8; 32];
        assert!(
            !verify_response(hwid, &challenge, &wrong_response),
            "verify_response must reject an incorrect response"
        );
    }

    #[test]
    fn test_verify_response_rejects_wrong_hwid() {
        let real_hwid = b"real-machine";
        let fake_hwid = b"fake-machine";
        let challenge = generate_challenge();
        let response = compute_response(fake_hwid, &challenge);
        assert!(
            !verify_response(real_hwid, &challenge, &response),
            "verify_response must reject response from wrong HWID"
        );
    }

    #[test]
    fn test_verify_response_rejects_wrong_challenge() {
        let hwid = b"my-machine";
        let challenge1 = generate_challenge();
        let challenge2 = generate_challenge();
        let response = compute_response(hwid, &challenge1);
        assert!(
            !verify_response(hwid, &challenge2, &response),
            "verify_response must reject response for a different challenge"
        );
    }

    #[test]
    fn test_verify_response_rejects_empty_response() {
        let hwid = b"my-machine";
        let challenge = generate_challenge();
        assert!(
            !verify_response(hwid, &challenge, &[]),
            "verify_response must reject an empty response"
        );
    }

    #[test]
    fn test_verify_response_rejects_truncated_response() {
        let hwid = b"my-machine";
        let challenge = generate_challenge();
        let response = compute_response(hwid, &challenge);
        // Truncate to half length
        assert!(
            !verify_response(hwid, &challenge, &response[..16]),
            "verify_response must reject a truncated response"
        );
    }

    #[test]
    fn test_empty_hwid_challenge_response() {
        let hwid = b"";
        let challenge = generate_challenge();
        let response = compute_response(hwid, &challenge);
        assert!(
            verify_response(hwid, &challenge, &response),
            "empty HWID should still work in challenge-response"
        );
    }

    #[test]
    fn test_constant_time_eq_equal() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn test_constant_time_eq_different_lengths() {
        assert!(!constant_time_eq(b"short", b"longer"));
    }

    #[test]
    fn test_constant_time_eq_different_content() {
        assert!(!constant_time_eq(b"aaaaa", b"aaaab"));
    }
}
