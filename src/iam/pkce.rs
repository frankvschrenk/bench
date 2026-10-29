//! PKCE (RFC 7636) S256 verification.
//!
//! The client sends a code_challenge at /auth and the matching
//! code_verifier at /token; we recompute BASE64URL(SHA256(verifier)) and
//! compare. S256 is the only method accepted — public clients must never
//! fall back to the plain method.

use data_encoding::BASE64URL_NOPAD;
use sha2::{Digest, Sha256};

/// Returns true when SHA256(verifier), base64url-encoded (no padding),
/// equals the challenge presented during authorization.
pub fn verify_s256(verifier: &str, challenge: &str) -> bool {
    let digest = Sha256::digest(verifier.as_bytes());
    let computed = BASE64URL_NOPAD.encode(&digest);
    timing_safe_eq(computed.as_bytes(), challenge.as_bytes())
}

/// Constant-time comparison so a failed verification does not leak how
/// many leading characters matched.
fn timing_safe_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_rfc7636_example() {
        // The worked example from RFC 7636 Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(verify_s256(verifier, challenge));
    }

    #[test]
    fn rejects_wrong_verifier() {
        assert!(!verify_s256("not-the-verifier", "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"));
    }
}
