//! Postgres-backed user + group store for oosiam.
//!
//! One table in its own `iam` schema. Groups are a text[] on the user
//! row rather than a join table: the data is tiny and the group strings
//! ARE the contract — oos derives roles by splitting them on "-" (e.g.
//! "oos-admin") — so a normalised group entity would buy nothing here.
//! Passwords are hashed with argon2id; plaintext is never stored or
//! compared. The PHC string format interoperates with Bun.password, so
//! a users table seeded by the Bun version verifies unchanged.
//!
//! ensure_schema is idempotent so a fresh checkout boots without a
//! migration step, matching the rest of the stack. Rows are mapped by
//! hand via Row::try_get (the workspace sqlx omits the derive feature).

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::Serialize;
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};

/// A stored user. The password hash is never part of this shape.
#[derive(Clone, Serialize)]
pub struct IamUser {
    pub id: i32,
    pub email: String,
    pub username: String,
    pub groups: Vec<String>,
}

/// Maps a row carrying (id, email, username, groups) to an IamUser.
fn row_to_user(row: &PgRow) -> Result<IamUser, sqlx::Error> {
    Ok(IamUser {
        id: row.try_get("id")?,
        email: row.try_get("email")?,
        username: row.try_get("username")?,
        groups: row.try_get("groups")?,
    })
}

/// Creates the iam schema + users table if absent.
pub async fn ensure_schema(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::query("CREATE SCHEMA IF NOT EXISTS iam").execute(pool).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS iam.users (\n            id            serial PRIMARY KEY,\n            email         varchar(320) UNIQUE NOT NULL,\n            username      varchar(200) NOT NULL,\n            password_hash text         NOT NULL,\n            groups        text[]       NOT NULL DEFAULT '{}',\n            created_at    timestamptz  NOT NULL DEFAULT now()\n        )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Returns the user when email + password match, else None. The hash
/// never leaves this module.
pub async fn verify_login(pool: &PgPool, email: &str, password: &str) -> anyhow::Result<Option<IamUser>> {
    let row = sqlx::query("SELECT id, email, username, groups, password_hash FROM iam.users WHERE email = $1")
        .bind(email)
        .fetch_optional(pool)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let hash: String = row.try_get("password_hash")?;
    if !verify_password(password, &hash) {
        return Ok(None);
    }
    Ok(Some(row_to_user(&row)?))
}

/// Returns every user (no hashes) for the oosd admin panel.
pub async fn list_users(pool: &PgPool) -> anyhow::Result<Vec<IamUser>> {
    let rows = sqlx::query("SELECT id, email, username, groups FROM iam.users ORDER BY id")
        .fetch_all(pool)
        .await?;
    let users = rows.iter().map(row_to_user).collect::<Result<Vec<_>, _>>()?;
    Ok(users)
}

/// Inserts a user with an argon2id-hashed password.
pub async fn create_user(
    pool: &PgPool,
    email: &str,
    username: &str,
    password: &str,
    groups: &[String],
) -> anyhow::Result<IamUser> {
    let hash = hash_password(password)?;
    let row = sqlx::query(
        "INSERT INTO iam.users (email, username, password_hash, groups) \
         VALUES ($1, $2, $3, $4) RETURNING id, email, username, groups",
    )
    .bind(email)
    .bind(username)
    .bind(hash)
    .bind(groups.to_vec())
    .fetch_one(pool)
    .await?;
    Ok(row_to_user(&row)?)
}

/// Changes a user's email (the login name) and username. The email stays
/// unique; a clash comes back as a readable error instead of a raw SQL one.
pub async fn update_user(pool: &PgPool, id: i32, email: &str, username: &str) -> anyhow::Result<IamUser> {
    let email = email.trim();
    let username = if username.trim().is_empty() { email } else { username.trim() };
    if email.is_empty() {
        anyhow::bail!("email must not be empty");
    }
    let row = sqlx::query("UPDATE iam.users SET email = $1, username = $2 WHERE id = $3 RETURNING id, email, username, groups")
        .bind(email)
        .bind(username)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(d) if d.is_unique_violation() => anyhow::anyhow!("email “{email}” is already taken"),
            _ => anyhow::Error::from(e),
        })?;
    match row {
        Some(r) => Ok(row_to_user(&r)?),
        None => anyhow::bail!("user {id} not found"),
    }
}

/// Replaces a user's group list.
pub async fn set_groups(pool: &PgPool, id: i32, groups: &[String]) -> anyhow::Result<()> {
    sqlx::query("UPDATE iam.users SET groups = $1 WHERE id = $2")
        .bind(groups.to_vec())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Replaces a user's password with a fresh argon2id hash.
pub async fn set_password(pool: &PgPool, id: i32, password: &str) -> anyhow::Result<()> {
    let hash = hash_password(password)?;
    sqlx::query("UPDATE iam.users SET password_hash = $1 WHERE id = $2")
        .bind(hash)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Removes a user by id. Idempotent.
pub async fn delete_user(pool: &PgPool, id: i32) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM iam.users WHERE id = $1").bind(id).execute(pool).await?;
    Ok(())
}

/// Creates a single admin user on the very first start (empty table)
/// with a random password returned to the caller (which prints it once
/// to stdout), so a solo installer can log in immediately. Returns None
/// when users already exist.
pub async fn seed_admin_if_empty(pool: &PgPool) -> anyhow::Result<Option<(String, String)>> {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM iam.users").fetch_one(pool).await?;
    if count > 0 {
        return Ok(None);
    }
    let email = "admin@oos.local".to_string();
    let password = random_password();
    create_user(pool, &email, "Admin", &password, &["oos-admin".to_string()]).await?;
    Ok(Some((email, password)))
}

/// Hashes a password with argon2id (the Argon2::default variant),
/// producing a PHC string that interoperates with Bun.password.
fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2 hash failed: {e}"))?;
    Ok(hash.to_string())
}

/// Verifies a password against a stored PHC hash. Any parse/verify
/// failure is a non-match, never an error to the caller.
fn verify_password(password: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok(),
        Err(_) => false,
    }
}

/// 16 hex characters from the OS CSPRNG — enough entropy for a one-time
/// first-run admin password the installer is told to rotate.
fn random_password() -> String {
    let mut bytes = [0u8; 8];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
