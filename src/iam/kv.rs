//! Publishes oosiam's connection config into JetStream KV.
//!
//! Why this exists: the project is moving toward a single bootstrap
//! input — the NATS URL — with every other setting pulled from KV at
//! runtime. oosiam, as the built-in default IdP, takes the first step by
//! publishing how to reach it under the `oos-iam` bucket, so a fresh oos
//! needs no auth settings entered by hand: it reads `oos-iam/user` and
//! points its PKCE flow at the issuer found there.
//!
//! This is the producer half only. oos-svc's kv module is config-map
//! specific (one flat object under the key `config`), which does not fit
//! a single JSON value under `user`, so the bucket bind + put is inlined
//! here with the same history depth (5) the rest of the stack uses.

use async_nats::jetstream::{self, kv::Config as KvConfig};
use async_nats::Client;
use serde::Serialize;

const BUCKET: &str = "oos-iam";
const KEY: &str = "user";
const HISTORY: i64 = 5;

/// Config a client needs to drive the OAuth2 + PKCE flow against oosiam.
/// Nothing here is secret — the RS256 private key never leaves oosiam —
/// so it is stored as-is.
#[derive(Serialize)]
pub struct IamClientConfig {
    /// OIDC issuer URL — discovery, token `iss`, and jwks_uri derive from it.
    #[serde(rename = "issuerUrl")]
    pub issuer_url: String,
    /// Default OAuth2 client_id oosiam expects from the desktop client.
    #[serde(rename = "clientId")]
    pub client_id: String,
    /// Loopback redirect URI for the desktop PKCE callback.
    #[serde(rename = "redirectUri")]
    pub redirect_uri: String,
}

/// Ensures the `oos-iam` bucket exists and writes the `user` entry with
/// the current connection config. Overwrites on every boot so the
/// published issuer always matches the instance actually running.
pub async fn publish_config(client: &Client, cfg: &IamClientConfig) -> anyhow::Result<()> {
    let js = jetstream::new(client.clone());
    let store = match js.get_key_value(BUCKET).await {
        Ok(store) => store,
        Err(_) => {
            js.create_key_value(KvConfig {
                bucket: BUCKET.to_string(),
                history: HISTORY,
                ..Default::default()
            })
            .await?
        }
    };
    let bytes = serde_json::to_vec(cfg)?;
    store.put(KEY, bytes.into()).await?;
    Ok(())
}
