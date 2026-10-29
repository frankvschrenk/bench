//! Benutzer-Abgleich für Anwendungen wie waki: ein Dienstkonto per
//! `client_credentials` und ein schmaler Ausschnitt der Keycloak-Admin-API,
//! damit dieselbe Anwendung im Betrieb mit Keycloak und in der Entwicklung mit
//! der Bench spricht. Dazu ein Anstoß nach jeder Benutzeränderung.
//!
//!   POST /token  grant_type=client_credentials, client_id=waki-sync,
//!                client_secret=… (oder HTTP Basic)
//!                → { access_token (undurchsichtig, 5 min), token_type, expires_in, scope }
//!   GET  /admin/realms/{realm}/users?first=&max=&search=&email=
//!                → [{ id, username, email, firstName, lastName, enabled }]
//!
//! Geheimnis: `<Datenordner>/iam-waki-sync.secret` – beim ersten Start
//! erzeugt, nur für den Besitzer lesbar; die Anwendung liest die Datei
//! (waki: WAKI_DIRECTORY_SECRET_FILE), abgetippt wird nichts.
//!
//! Anstoß: `<Datenordner>/iam-hooks.txt`, je Zeile `URL GEHEIMNIS`. Nach
//! Anlegen, Gruppenänderung oder Löschen geht ein `POST URL` mit Kopfzeile
//! `X-Directory-Secret: GEHEIMNIS` raus (waki: …/directory/changed). Fehlt
//! die Datei, passiert nichts – der regelmäßige Abgleich holt es nach.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::rngs::OsRng;
use rand::RngCore;

pub const CLIENT: &str = "waki-sync";
pub const SCOPE: &str = "users:read";
pub const TOKEN_TTL_SEC: u64 = 300;

pub struct SyncAccess {
    secret: String,
    /// ausgegebene Tokens -> Ablauf (Sekunden)
    tokens: Mutex<HashMap<String, u64>>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Vergleich in konstanter Zeit (Länge darf verraten werden).
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl SyncAccess {
    /// Geheimnis aus der Datei, beim ersten Mal erzeugen (Rechte 600).
    pub fn load_or_create(path: &Path) -> anyhow::Result<Self> {
        let secret = match std::fs::read_to_string(path) {
            Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => {
                let s = random_hex(32);
                write_private(path, &s)?;
                eprintln!("[bench/iam] Dienstkonto {CLIENT}: Geheimnis neu in {}", path.display());
                s
            }
        };
        Ok(SyncAccess { secret, tokens: Mutex::new(HashMap::new()) })
    }

    pub fn check_client(&self, client_id: &str, secret: &str) -> bool {
        client_id == CLIENT && same(secret.trim().as_bytes(), self.secret.as_bytes())
    }

    pub fn issue(&self) -> String {
        let tok = random_hex(32);
        let t = now();
        let mut m = self.tokens.lock().unwrap();
        m.retain(|_, exp| *exp > t);
        m.insert(tok.clone(), t + TOKEN_TTL_SEC);
        tok
    }

    pub fn valid(&self, tok: &str) -> bool {
        self.tokens.lock().unwrap().get(tok).is_some_and(|exp| *exp > now())
    }
}

#[cfg(unix)]
fn write_private(path: &Path, s: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(s.as_bytes())?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, s: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, s)?;
    Ok(())
}

// ── Anstoß nach Änderungen ─────────────────────────────────────────────

static HOOKS: OnceLock<PathBuf> = OnceLock::new();

pub fn set_hooks_file(path: PathBuf) {
    let _ = HOOKS.set(path);
}

/// Benutzer haben sich geändert: alle eingetragenen Empfänger anstoßen
/// (im Hintergrund; Fehler nur im Log).
pub fn changed() {
    let Some(path) = HOOKS.get().cloned() else { return };
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    let targets: Vec<(String, String)> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.to_string(), it.next().unwrap_or("").to_string()))
        })
        .collect();
    if targets.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let client = match reqwest::Client::builder().timeout(Duration::from_secs(5)).build() {
            Ok(c) => c,
            Err(_) => return,
        };
        for (url, secret) in targets {
            let res = client
                .post(&url)
                .header("X-Directory-Secret", secret)
                .json(&serde_json::json!({ "event": "users-changed" }))
                .send()
                .await;
            match res {
                Ok(r) if r.status().is_success() => eprintln!("[bench/iam] Anstoß an {url}: {}", r.status()),
                Ok(r) => eprintln!("[bench/iam] Anstoß an {url} abgelehnt: {}", r.status()),
                Err(e) => eprintln!("[bench/iam] Anstoß an {url} fehlgeschlagen: {e}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_file_tokens_and_client_check() {
        let dir = std::env::temp_dir().join(format!("iam-sync-{}", random_hex(4)));
        let path = dir.join("iam-waki-sync.secret");
        let a = SyncAccess::load_or_create(&path).unwrap();
        let b = SyncAccess::load_or_create(&path).unwrap();
        assert_eq!(a.secret, b.secret, "zweiter Start liest dasselbe Geheimnis");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(a.check_client(CLIENT, &a.secret));
        assert!(!a.check_client(CLIENT, "falsch"));
        assert!(!a.check_client("oos-desktop", &a.secret));
        let t = a.issue();
        assert!(a.valid(&t) && !a.valid("nix"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
