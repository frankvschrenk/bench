//! RS256 signing key management (load-or-generate) + public JWK.
//!
//! oosiam signs tokens with RS256 so resource servers verify them
//! against the published JWKS without a shared secret. The private key
//! is generated on first start and persisted as PKCS#8 PEM, so the key
//! — and every token already issued — stays valid across restarts. Point
//! OOSIAM_KEY_PATH at a stable location outside the repo for a real
//! deployment. The PEM is byte-compatible with the Bun version's, so an
//! existing signing-key.pem keeps working and produces the same kid.

use std::fs;
use std::path::Path;

use data_encoding::BASE64URL_NOPAD;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rsa::traits::PublicKeyParts;
use rsa::{RsaPrivateKey, RsaPublicKey};
use serde::Serialize;
use sha2::{Digest, Sha256};

const ALG: &str = "RS256";
const BITS: usize = 2048;

/// The public JWK published at /jwks (one entry in the key set).
#[derive(Clone, Serialize)]
pub struct PublicJwk {
    pub kty: String,
    pub n: String,
    pub e: String,
    pub alg: String,
    #[serde(rename = "use")]
    pub use_: String,
    pub kid: String,
}

/// Signing material held in memory for the process lifetime.
pub struct SigningKey {
    pub private_key: RsaPrivateKey,
    pub public_jwk: PublicJwk,
    /// Key id, the RFC 7638 thumbprint of the public key — stable across
    /// restarts because it is derived purely from the key material.
    pub kid: String,
}

/// Returns the signing key, generating + persisting a fresh RSA keypair
/// the first time. Subsequent starts reload the same key so previously
/// issued tokens remain verifiable.
pub fn load_or_generate(key_path: &str) -> anyhow::Result<SigningKey> {
    let private_key = if Path::new(key_path).exists() {
        let pem = fs::read_to_string(key_path)?;
        RsaPrivateKey::from_pkcs8_pem(&pem)?
    } else {
        // ThreadRng is a CSPRNG; fine for one-time key generation.
        let mut rng = rand::thread_rng();
        let key = RsaPrivateKey::new(&mut rng, BITS)?;
        let pem = key.to_pkcs8_pem(LineEnding::LF)?;
        if let Some(dir) = Path::new(key_path).parent() {
            if !dir.as_os_str().is_empty() {
                fs::create_dir_all(dir)?;
            }
        }
        fs::write(key_path, pem.as_bytes())?;
        eprintln!("[bench/iam] generated new RS256 signing key at {key_path}");
        key
    };
    derive(private_key)
}

/// Builds the public JWK + kid from a private key.
fn derive(private_key: RsaPrivateKey) -> anyhow::Result<SigningKey> {
    let pub_key = RsaPublicKey::from(&private_key);
    let n = BASE64URL_NOPAD.encode(&pub_key.n().to_bytes_be());
    let e = BASE64URL_NOPAD.encode(&pub_key.e().to_bytes_be());
    let kid = thumbprint(&n, &e);
    let public_jwk = PublicJwk {
        kty: "RSA".into(),
        n: n.clone(),
        e: e.clone(),
        alg: ALG.into(),
        use_: "sig".into(),
        kid: kid.clone(),
    };
    Ok(SigningKey { private_key, public_jwk, kid })
}

/// RFC 7638 JWK thumbprint: BASE64URL(SHA256(canonical JSON)). For an
/// RSA key the canonical form is the members e, kty, n in lexicographic
/// order with no whitespace. Matches jose's calculateJwkThumbprint, so a
/// key file written by the Bun version yields an identical kid.
fn thumbprint(n: &str, e: &str) -> String {
    let canonical = format!("{{\"e\":\"{e}\",\"kty\":\"RSA\",\"n\":\"{n}\"}}");
    BASE64URL_NOPAD.encode(&Sha256::digest(canonical.as_bytes()))
}
