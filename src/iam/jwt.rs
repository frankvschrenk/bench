//! Issue RS256-signed access / ID tokens.
//!
//! The claim shape is fixed by Onisin's existing security contract — the
//! exact set oos's auth-claims decoder expects: sub, email,
//! preferred_username, groups, plus standard iss/aud/iat/exp. oosiam
//! uses one JWT for both access_token and id_token because the desktop
//! public client treats them identically; the claim set therefore
//! carries both the identity (email/username) and the authorization
//! input (groups, from which oos derives the group header).
//!
//! The JWT is assembled by hand rather than via a framework: it is three
//! base64url segments and one RSASSA-PKCS1-v1_5/SHA-256 signature, which
//! is smaller and more auditable than pulling a JWT crate for one token
//! shape. RS256 = PKCS1-v1.5 over SHA-256.

use std::time::{SystemTime, UNIX_EPOCH};

use data_encoding::BASE64URL_NOPAD;
use rsa::pkcs1v15::SigningKey as RsaSigningKey;
use rsa::signature::{SignatureEncoding, Signer};
use serde::Serialize;
use sha2::Sha256;

use super::keys::SigningKey;

/// Identity that goes into a token.
pub struct TokenSubject {
    pub email: String,
    pub username: String,
    pub groups: Vec<String>,
}

#[derive(Serialize)]
struct Header<'a> {
    alg: &'a str,
    kid: &'a str,
    typ: &'a str,
}

#[derive(Serialize)]
struct Claims<'a> {
    sub: &'a str,
    email: &'a str,
    preferred_username: &'a str,
    groups: &'a [String],
    iss: &'a str,
    aud: &'a str,
    iat: u64,
    exp: u64,
}

/// Signs an RS256 JWT for one user. `issuer` is the oosiam issuer URL,
/// `audience` the requesting client_id, `ttl_sec` the lifetime.
pub fn sign_token(
    key: &SigningKey,
    issuer: &str,
    audience: &str,
    sub: &TokenSubject,
    ttl_sec: u64,
) -> anyhow::Result<String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let header = Header { alg: "RS256", kid: &key.kid, typ: "JWT" };
    let claims = Claims {
        sub: &sub.email,
        email: &sub.email,
        preferred_username: &sub.username,
        groups: &sub.groups,
        iss: issuer,
        aud: audience,
        iat: now,
        exp: now + ttl_sec,
    };

    let h = BASE64URL_NOPAD.encode(&serde_json::to_vec(&header)?);
    let c = BASE64URL_NOPAD.encode(&serde_json::to_vec(&claims)?);
    let signing_input = format!("{h}.{c}");

    let signer = RsaSigningKey::<Sha256>::new(key.private_key.clone());
    let signature = signer.sign(signing_input.as_bytes());
    let s = BASE64URL_NOPAD.encode(&signature.to_vec());

    Ok(format!("{signing_input}.{s}"))
}
