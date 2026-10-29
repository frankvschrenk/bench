//! NATS Request-Reply management API for oosiam.
//!
//! oosd's admin panel manages users and groups over NATS; the OAuth HTTP
//! endpoints stay focused on the login/token flow. These subjects are
//! part of the trusted local control plane (same trust level as the rest
//! of oos.cmd.*), so they are not separately authenticated here — if that
//! trust boundary ever changes, this is the one place to gate.
//!
//! Subjects (queue group `oosiam-cmd` so a request is handled once even
//! with several subscribers):
//!
//!   oos.cmd.oosiam.user.list         → { users: IamUser[] }
//!   oos.cmd.oosiam.user.create       ← { email, username, password, groups? }
//!                                     → { ok: true, user } | { ok: false, error }
//!   oos.cmd.oosiam.user.update       ← { id, email, username? } → { ok, user } | { ok: false, error }
//!   oos.cmd.oosiam.user.set_groups   ← { id, groups? }      → { ok } | { ok: false, error }
//!   oos.cmd.oosiam.user.set_password ← { id, password }     → { ok } | { ok: false, error }
//!   oos.cmd.oosiam.user.delete       ← { id }               → { ok } | { ok: false, error }
//!
//! Errors reply in-band as { ok: false, error }. The Bun version also
//! fanned errors out to oos.error / oosiam.log; that bridge (oosl) is
//! not ported, so the fan-out is intentionally omitted, matching the
//! oosai-rs handler.

use async_nats::{Client, Subscriber};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;

use super::store;

const QUEUE: &str = "oosiam-cmd";

const SUBJECTS: &[&str] = &[
    "oos.cmd.oosiam.user.list",
    "oos.cmd.oosiam.user.create",
    "oos.cmd.oosiam.user.update",
    "oos.cmd.oosiam.user.set_groups",
    "oos.cmd.oosiam.user.set_password",
    "oos.cmd.oosiam.user.delete",
];

/// Subscribes the user-management subjects and spawns their dispatch
/// loops. Returns once all subscriptions are live.
pub async fn serve(client: Client, pool: PgPool) -> anyhow::Result<()> {
    for subject in SUBJECTS {
        let sub = client.queue_subscribe(*subject, QUEUE.to_string()).await?;
        tokio::spawn(run_subject(client.clone(), pool.clone(), sub));
        eprintln!("[bench/iam] subscribed to {subject}");
    }
    Ok(())
}

async fn run_subject(client: Client, pool: PgPool, mut sub: Subscriber) {
    while let Some(msg) = sub.next().await {
        // One task per message so a slow query never blocks the next
        // request on the same subject.
        let client = client.clone();
        let pool = pool.clone();
        tokio::spawn(async move {
            let Some(reply) = msg.reply.clone() else { return };
            let result = handle(&pool, msg.subject.as_str(), msg.payload.as_ref()).await;
            let value = result.unwrap_or_else(|e| json!({ "ok": false, "error": e.to_string() }));
            if let Ok(bytes) = serde_json::to_vec(&value) {
                let _ = client.publish(reply, bytes.into()).await;
            }
        });
    }
}

async fn handle(pool: &PgPool, subject: &str, payload: &[u8]) -> anyhow::Result<Value> {
    match subject {
        "oos.cmd.oosiam.user.list" => {
            let users = store::list_users(pool).await?;
            Ok(json!({ "users": users }))
        }
        "oos.cmd.oosiam.user.create" => {
            #[derive(Deserialize)]
            struct Req {
                email: String,
                username: String,
                password: String,
                #[serde(default)]
                groups: Vec<String>,
            }
            let r: Req = serde_json::from_slice(payload)?;
            let user = store::create_user(pool, &r.email, &r.username, &r.password, &r.groups).await?;
            super::sync::changed();
            Ok(json!({ "ok": true, "user": user }))
        }
        "oos.cmd.oosiam.user.update" => {
            #[derive(Deserialize)]
            struct Req {
                id: i32,
                email: String,
                #[serde(default)]
                username: String,
            }
            let r: Req = serde_json::from_slice(payload)?;
            let user = store::update_user(pool, r.id, &r.email, &r.username).await?;
            super::sync::changed();
            Ok(json!({ "ok": true, "user": user }))
        }
        "oos.cmd.oosiam.user.set_groups" => {
            #[derive(Deserialize)]
            struct Req {
                id: i32,
                #[serde(default)]
                groups: Vec<String>,
            }
            let r: Req = serde_json::from_slice(payload)?;
            store::set_groups(pool, r.id, &r.groups).await?;
            super::sync::changed();
            Ok(json!({ "ok": true }))
        }
        "oos.cmd.oosiam.user.set_password" => {
            #[derive(Deserialize)]
            struct Req {
                id: i32,
                password: String,
            }
            let r: Req = serde_json::from_slice(payload)?;
            store::set_password(pool, r.id, &r.password).await?;
            Ok(json!({ "ok": true }))
        }
        "oos.cmd.oosiam.user.delete" => {
            #[derive(Deserialize)]
            struct Req {
                id: i32,
            }
            let r: Req = serde_json::from_slice(payload)?;
            store::delete_user(pool, r.id).await?;
            super::sync::changed();
            Ok(json!({ "ok": true }))
        }
        other => Err(anyhow::anyhow!("unknown subject: {other}")),
    }
}
