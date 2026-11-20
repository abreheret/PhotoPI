//! Password login and access tokens.
//!
//! The Immich Android app sends the token as the `immich_access_token` cookie (native OkHttp client);
//! the `x-immich-user-token` header and `Authorization: Bearer` are accepted too. Each login creates a
//! session; sync checkpoints belong to the session.

use crate::db::{self, User, USER_COLS};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::{FromRequestParts, State};
use axum::http::header::{AUTHORIZATION, COOKIE, SET_COOKIE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{AppendHeaders, IntoResponse};
use axum::Json;
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const TOKEN_COOKIE: &str = "immich_access_token";
const COOKIE_MAX_AGE_S: i64 = 400 * 24 * 3600;

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::encode_b64(&rand::random::<[u8; 16]>()).map_err(|e| anyhow::anyhow!("salt: {e}"))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("hash: {e}"))
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
        .unwrap_or(false)
}

fn new_token() -> String {
    let bytes: [u8; 32] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn token_hash(token: &str) -> String {
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// The token of a request: header, bearer or cookie.
fn token_from(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("x-immich-user-token").and_then(|v| v.to_str().ok()) {
        return Some(v.trim().to_string());
    }
    if let Some(v) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(t) = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")) {
            return Some(t.trim().to_string());
        }
    }
    for cookie_header in headers.get_all(COOKIE) {
        let Ok(s) = cookie_header.to_str() else { continue };
        for part in s.split(';') {
            if let Some((k, v)) = part.trim().split_once('=') {
                if k == TOKEN_COOKIE && !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// An authenticated request: the user and the session its token belongs to.
#[derive(Debug, Clone)]
pub struct Auth {
    pub user: User,
    pub session_id: String,
}

impl FromRequestParts<AppState> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        let token = token_from(&parts.headers).ok_or_else(ApiError::unauthorized)?;
        let hash = token_hash(&token);
        st.db(move |c| {
            let sql = format!(
                "SELECT s.id, {} FROM sessions s JOIN users u ON u.id = s.user_id WHERE s.token_hash = ?1",
                USER_COLS.split(", ").map(|col| format!("u.{col}")).collect::<Vec<_>>().join(", ")
            );
            let found = c
                .query_row(&sql, [&hash], |r| {
                    let session_id: String = r.get(0)?;
                    // user columns start at index 1
                    let user = User {
                        id: r.get(1)?,
                        email: r.get(2)?,
                        name: r.get(3)?,
                        is_admin: r.get(4)?,
                        storage_label: r.get(5)?,
                        created_at: r.get(6)?,
                        updated_at: r.get(7)?,
                        profile_changed_at: r.get(8)?,
                        update_id: r.get(9)?,
                    };
                    Ok(Auth { user, session_id })
                })
                .optional()?;
            found.ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "Invalid user token"))
        })
        .await
    }
}

#[derive(Deserialize)]
pub struct LoginDto {
    email: String,
    password: String,
}

pub async fn login(State(st): State<AppState>, headers: HeaderMap, Json(dto): Json<LoginDto>) -> ApiResult<impl IntoResponse> {
    let device_type = header_str(&headers, "deviceType");
    let device_os = header_str(&headers, "deviceModel");
    let email = dto.email.trim().to_string();
    let (user, token) = st
        .db(move |c| {
            let Some((user, hash)) = db::user_by_email(c, &email)? else {
                // Same work as a real check, so timing does not reveal which emails exist.
                static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
                let dummy = DUMMY.get_or_init(|| hash_password("photopi-dummy").unwrap_or_default());
                verify_password(&dto.password, dummy);
                return Err(ApiError::new(StatusCode::UNAUTHORIZED, "Incorrect email or password"));
            };
            if !verify_password(&dto.password, &hash) {
                return Err(ApiError::new(StatusCode::UNAUTHORIZED, "Incorrect email or password"));
            }
            let token = new_token();
            let now = db::now_ms();
            c.execute(
                "INSERT INTO sessions(id, user_id, token_hash, device_type, device_os, created_at, updated_at) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                rusqlite::params![uuid::Uuid::new_v4().to_string(), user.id, token_hash(&token), device_type, device_os, now],
            )?;
            Ok((user, token))
        })
        .await?;
    tracing::info!("login: {}", user.email);
    let cookies = [
        format!("{TOKEN_COOKIE}={token}; Path=/; Max-Age={COOKIE_MAX_AGE_S}; HttpOnly; SameSite=Lax"),
        format!("immich_auth_type=password; Path=/; Max-Age={COOKIE_MAX_AGE_S}; HttpOnly; SameSite=Lax"),
        format!("immich_is_authenticated=true; Path=/; Max-Age={COOKIE_MAX_AGE_S}; SameSite=Lax"),
    ];
    let body = json!({
        "accessToken": token,
        "userId": user.id,
        "userEmail": user.email,
        "name": user.name,
        "isAdmin": user.is_admin,
        "profileImagePath": "",
        "shouldChangePassword": false,
        "isOnboarded": true,
    });
    Ok((StatusCode::CREATED, AppendHeaders(cookies.map(|c| (SET_COOKIE, c))), Json(body)))
}

fn header_str(headers: &HeaderMap, name: &str) -> String {
    headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("").chars().take(100).collect()
}

pub async fn logout(State(st): State<AppState>, auth: Auth) -> ApiResult<impl IntoResponse> {
    let sid = auth.session_id.clone();
    st.db(move |c| {
        c.execute("DELETE FROM sessions WHERE id = ?1", [sid])?;
        Ok(())
    })
    .await?;
    let cookies = [
        format!("{TOKEN_COOKIE}=; Path=/; Max-Age=0"),
        "immich_auth_type=; Path=/; Max-Age=0".to_string(),
        "immich_is_authenticated=; Path=/; Max-Age=0".to_string(),
    ];
    Ok((
        AppendHeaders(cookies.map(|c| (SET_COOKIE, c))),
        Json(json!({ "successful": true, "redirectUri": "/auth/login" })),
    ))
}

pub async fn validate_token(_auth: Auth) -> Json<Value> {
    Json(json!({ "authStatus": true }))
}

pub async fn status(_auth: Auth) -> Json<Value> {
    Json(json!({ "password": true, "pinCode": false, "isElevated": false }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn password_roundtrip() {
        let h = hash_password("secret").unwrap();
        assert!(verify_password("secret", &h));
        assert!(!verify_password("nope", &h));
        assert!(!verify_password("secret", "garbage"));
    }

    #[test]
    fn token_sources() {
        let mut h = HeaderMap::new();
        assert_eq!(token_from(&h), None);
        h.insert(COOKIE, HeaderValue::from_static("a=1; immich_access_token=abc; b=2"));
        assert_eq!(token_from(&h).as_deref(), Some("abc"));
        h.insert(AUTHORIZATION, HeaderValue::from_static("Bearer xyz"));
        assert_eq!(token_from(&h).as_deref(), Some("xyz"));
        h.insert("x-immich-user-token", HeaderValue::from_static("hdr"));
        assert_eq!(token_from(&h).as_deref(), Some("hdr"));
    }
}
