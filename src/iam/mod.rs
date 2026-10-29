//! Dev-IdP der Bench (vorher der eigene Dienst services/oosiam).
//
// Ein minimaler OAuth2/OIDC-Server (authorization code + PKCE, RS256, JWKS),
// damit man in der Entwicklung keinen Keycloak braucht. In Firmen gibt es
// einen eigenen IdP – oos, waki & Co. kennen nur die Issuer-URL, deshalb ist
// das hier reines Dev-Werkzeug und gehört in die Bench.
//
// Der Draht-Vertrag ist unverändert, damit nichts umgestellt werden muss:
//   HTTP  http://localhost:5556  /.well-known/openid-configuration, /jwks, /auth, /token
//   NATS  oos.cmd.oosiam.user.{list,create,set_groups,set_password,delete}
//   KV    oos-iam/user  {issuerUrl, clientId, redirectUri}
// Benutzer liegen in Postgres (Schema iam in der App-DB aus Settings →
// Database), der Signierschlüssel im Datenordner der Bench.
//
// Die Bench-Oberfläche (Users) spricht über die Funktionen unten direkt
// mit dem Store; die NATS-Subjects bleiben für andere Werkzeuge bestehen.

mod jwt;
mod keys;
mod kv;
mod nats;
mod pkce;
mod server;
pub mod store;
mod sync;

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::sync::RwLock;

use crate::settings::BenchSettings;

const HOST: &str = "localhost";
const PORT: u16 = 5556;
/// Standard-Client für Desktop-Apps (oos); veröffentlicht im KV.
const CLIENT_ID: &str = "oos-desktop";
const REDIRECT_URI: &str = "http://localhost:5557/callback";
const TOKEN_TTL_SEC: u64 = 8 * 3600;
const CODE_TTL_SEC: u64 = 300;
/// App-DB, falls in Settings keine eingetragen ist (wie früher oosiam).
const DEFAULT_DB: &str = "onisin";

/// Laufzeit-Zustand für die Users-Ansicht.
#[derive(Default)]
pub struct IamState {
    pool: RwLock<Option<PgPool>>,
    status: RwLock<String>,
}

fn issuer() -> String {
    format!("http://{HOST}:{PORT}")
}

/// Startet den IdP: Postgres, Schema, erster Admin, Schlüssel, HTTP, NATS, KV.
/// Läuft einmal beim Hochfahren; Änderungen an den Settings brauchen einen
/// Neustart der Bench. Fehler legen nur den IdP still, nie die Bench.
pub async fn start(settings: BenchSettings, key_path: PathBuf, state: Arc<IamState>) {
    let set_status = |s: String| {
        let state = state.clone();
        async move {
            eprintln!("[bench/iam] {s}");
            *state.status.write().await = s;
        }
    };

    if settings.dsn.trim().is_empty() {
        set_status("aus – kein DSN unter Einstellungen → Datenbank".into()).await;
        return;
    }
    let db = if settings.app_database.trim().is_empty() { DEFAULT_DB } else { settings.app_database.trim() };
    let url = crate::db::rewrite_database(&settings.dsn, db);
    let pool = match PgPoolOptions::new().max_connections(5).connect(&url).await {
        Ok(p) => p,
        Err(e) => return set_status(format!("aus – Postgres nicht erreichbar: {e}")).await,
    };
    if let Err(e) = store::ensure_schema(&pool).await {
        return set_status(format!("aus – Schema iam: {e}")).await;
    }
    match store::seed_admin_if_empty(&pool).await {
        // Passwort nur einmal auf stderr, nie über NATS oder ins Fenster-Log
        Ok(Some((email, password))) => {
            eprintln!("\n[bench/iam] erster Admin: {email} / {password}");
            eprintln!("[bench/iam] Passwort wird nur jetzt angezeigt – anmelden und ändern.\n");
        }
        Ok(None) => {}
        Err(e) => eprintln!("[bench/iam] Admin anlegen fehlgeschlagen: {e}"),
    }
    let key = match keys::load_or_generate(&key_path.to_string_lossy()) {
        Ok(k) => k,
        Err(e) => return set_status(format!("aus – Signierschlüssel {}: {e}", key_path.display())).await,
    };
    let kid = key.kid.clone();
    *state.pool.write().await = Some(pool.clone());

    // Dienstkonto für den Benutzer-Abgleich (waki & Co.), Geheimnis neben dem Schlüssel
    let data_dir = key_path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let sync_access = match sync::SyncAccess::load_or_create(&data_dir.join("iam-waki-sync.secret")) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("[bench/iam] Dienstkonto {}: {e} – Abgleich aus", sync::CLIENT);
            None
        }
    };
    sync::set_hooks_file(data_dir.join("iam-hooks.txt"));

    // HTTP (OAuth2 + PKCE)
    {
        let (pool, state) = (pool.clone(), state.clone());
        tokio::spawn(async move {
            if let Err(e) = server::serve(HOST.into(), PORT, pool, key, issuer(), TOKEN_TTL_SEC, CODE_TTL_SEC, sync_access).await {
                let msg = format!("HTTP :{PORT} fehlgeschlagen (läuft noch ein alter oosiam?): {e}");
                eprintln!("[bench/iam] {msg}");
                *state.status.write().await = format!("gestört – {msg}");
            }
        });
    }

    // NATS-Verwaltung + KV – über den ersten konfigurierten Server
    let nats_note = match settings.servers.first() {
        None => "ohne NATS (kein Server eingetragen)".to_string(),
        Some(srv) => match async_nats::connect(&srv.url).await {
            Err(e) => format!("ohne NATS ({}: {e})", srv.url),
            Ok(client) => {
                if let Err(e) = nats::serve(client.clone(), pool.clone()).await {
                    eprintln!("[bench/iam] NATS-Subjects: {e}");
                }
                let cfg = kv::IamClientConfig {
                    issuer_url: issuer(),
                    client_id: CLIENT_ID.into(),
                    redirect_uri: REDIRECT_URI.into(),
                };
                if let Err(e) = kv::publish_config(&client, &cfg).await {
                    eprintln!("[bench/iam] KV oos-iam: {e}");
                }
                format!("NATS {}", srv.name)
            }
        },
    };
    set_status(format!("läuft – {} · {db} · {nats_note} · kid {kid}", issuer())).await;
}

// ── Funktionen für die Users-Ansicht ─────────────────────────────────

#[derive(Clone, Debug, Serialize)]
pub struct IamInfo {
    pub issuer: String,
    pub status: String,
    pub running: bool,
}

async fn pool(state: &IamState) -> Result<PgPool, String> {
    state
        .pool
        .read()
        .await
        .clone()
        .ok_or_else(|| format!("IAM läuft nicht: {}", state.status.try_read().map(|s| s.clone()).unwrap_or_default()))
}

fn err(e: anyhow::Error) -> String {
    e.to_string()
}

pub async fn iam_info(state: &IamState) -> IamInfo {
    IamInfo {
        issuer: issuer(),
        status: state.status.read().await.clone(),
        running: state.pool.read().await.is_some(),
    }
}

pub async fn iam_list(state: &IamState) -> Result<Vec<store::IamUser>, String> {
    store::list_users(&pool(state).await?).await.map_err(err)
}

pub async fn iam_create(
    state: &IamState,
    email: String,
    username: String,
    password: String,
    groups: Vec<String>,
) -> Result<store::IamUser, String> {
    let u = store::create_user(&pool(state).await?, &email, &username, &password, &groups).await.map_err(err)?;
    sync::changed();
    Ok(u)
}

pub async fn iam_update(state: &IamState, id: i32, email: String, username: String) -> Result<store::IamUser, String> {
    let u = store::update_user(&pool(state).await?, id, &email, &username).await.map_err(err)?;
    sync::changed();
    Ok(u)
}

pub async fn iam_set_groups(state: &IamState, id: i32, groups: Vec<String>) -> Result<(), String> {
    store::set_groups(&pool(state).await?, id, &groups).await.map_err(err)?;
    sync::changed();
    Ok(())
}

pub async fn iam_set_password(state: &IamState, id: i32, password: String) -> Result<(), String> {
    store::set_password(&pool(state).await?, id, &password).await.map_err(err)
}

pub async fn iam_delete(state: &IamState, id: i32) -> Result<(), String> {
    store::delete_user(&pool(state).await?, id).await.map_err(err)?;
    sync::changed();
    Ok(())
}
