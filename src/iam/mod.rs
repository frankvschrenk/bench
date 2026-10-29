//! Dev-IdP der Bench (vorher der eigene Dienst services/oosiam).
//
// Ein minimaler OAuth2/OIDC-Server (authorization code + PKCE, RS256, JWKS),
// damit man in der Entwicklung keinen Keycloak braucht. In Firmen gibt es
// einen eigenen IdP – oos, waki & Co. kennen nur die Issuer-URL, deshalb ist
// das hier reines Dev-Werkzeug und gehört in die Bench.
//
// Der Draht-Vertrag ist unverändert, damit nichts umgestellt werden muss:
//   HTTP  http://localhost:5556  /.well-known/openid-configuration, /jwks, /auth, /token
//         (Issuer einstellbar: Einstellungen → Allgemein → „Issuer-URL des Dev-IdP“;
//         anderer Host als localhost = lauscht auf 0.0.0.0, für Testrechner im Netz)
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

const DEFAULT_ISSUER: &str = "http://localhost:5556";
const DEFAULT_PORT: u16 = 5556;
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
    issuer: RwLock<String>,
}

/// Issuer aus den Einstellungen (ohne abschließenden Schrägstrich), leer = localhost.
pub fn issuer_of(s: &BenchSettings) -> String {
    let i = s.iam_issuer.trim().trim_end_matches('/');
    if i.is_empty() { DEFAULT_ISSUER.into() } else { i.into() }
}

/// Host und Port aus einer Issuer-URL „http(s)://host[:port][/pfad]“. None = keine gültige URL.
pub fn issuer_host_port(issuer: &str) -> Option<(String, u16)> {
    let rest = issuer.strip_prefix("http://").or_else(|| issuer.strip_prefix("https://"))?;
    let authority = rest.split('/').next()?;
    if authority.is_empty() {
        return None;
    }
    // [::1]:5556, host:5556, host
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (h, after) = v6.split_once(']')?;
        (format!("[{h}]"), after.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p)),
            None => (authority.to_string(), None),
        }
    };
    let port = match port {
        Some(p) => p.parse().ok()?,
        None => DEFAULT_PORT,
    };
    (!host.is_empty()).then_some((host, port))
}

/// Lauschadresse: localhost bleibt lokal, jeder andere Host lauscht im Netz.
fn bind_host(host: &str) -> &'static str {
    match host {
        "localhost" | "127.0.0.1" | "[::1]" => "localhost",
        _ => "0.0.0.0",
    }
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
    let issuer = issuer_of(&settings);
    let Some((host, port)) = issuer_host_port(&issuer) else {
        return set_status(format!("aus – Issuer-URL ungültig: {issuer}")).await;
    };
    let bind = bind_host(&host);
    *state.issuer.write().await = issuer.clone();
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
        let (pool, state, issuer) = (pool.clone(), state.clone(), issuer.clone());
        tokio::spawn(async move {
            if let Err(e) = server::serve(bind.into(), port, pool, key, issuer, TOKEN_TTL_SEC, CODE_TTL_SEC, sync_access).await {
                let msg = format!("HTTP {bind}:{port} fehlgeschlagen (läuft noch ein alter oosiam?): {e}");
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
                    issuer_url: issuer.clone(),
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
    set_status(format!("läuft – {issuer} (lauscht {bind}:{port}) · {db} · {nats_note} · kid {kid}")).await;
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
        issuer: state.issuer.read().await.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issuer_zerlegen() {
        assert_eq!(issuer_host_port("http://localhost:5556"), Some(("localhost".into(), 5556)));
        assert_eq!(issuer_host_port("http://192.168.1.71:5556"), Some(("192.168.1.71".into(), 5556)));
        assert_eq!(issuer_host_port("https://idp.example"), Some(("idp.example".into(), DEFAULT_PORT)));
        assert_eq!(issuer_host_port("http://[::1]:7000/realms/x"), Some(("[::1]".into(), 7000)));
        assert_eq!(issuer_host_port("localhost:5556"), None);
        assert_eq!(issuer_host_port("http://host:abc"), None);
        assert_eq!(issuer_host_port("http://"), None);
    }

    #[test]
    fn lauschadresse() {
        assert_eq!(bind_host("localhost"), "localhost");
        assert_eq!(bind_host("[::1]"), "localhost");
        assert_eq!(bind_host("192.168.1.71"), "0.0.0.0");
        let leer = BenchSettings::default();
        assert_eq!(issuer_of(&leer), "http://localhost:5556");
        let s = BenchSettings { iam_issuer: " http://mac.local:5556/ ".into(), ..Default::default() };
        assert_eq!(issuer_of(&s), "http://mac.local:5556");
    }
}
