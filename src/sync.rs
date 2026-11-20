//! The sync stream: how the app fills its local database (its timeline is built from it).
//!
//! `POST /sync/stream {types}` answers one JSON object per line: `{"type", "data", "ack"}`. The app stores a
//! batch, then sends the `ack` strings back (`POST /sync/ack`); the server keeps the last one per
//! (session, type) and next time only sends what changed after it. Acks are `"<Type>|<updateId>"`; the
//! updateId is opaque to the app, here a zero-padded value of the global change counter (db.rs).
//!
//! Only the types PhotoPI has data for are answered; the app gets nothing for albums, people, etc.
//! Reference implementation: immich `server/src/services/sync.service.ts`.

use crate::auth::Auth;
use crate::db::{self, USER_COLS};
use crate::dto::{self, asset_from_row, ASSET_COLS};
use crate::error::ApiResult;
use crate::state::AppState;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

const CHUNK: usize = 64 * 1024;

pub fn ack(kind: &str, update_id: i64) -> String {
    format!("{kind}|{update_id:020}")
}

/// updateId of an ack ("AssetV2|00000000000000000042" -> 42). Unknown format = from the start.
pub fn ack_update_id(ack: &str) -> i64 {
    ack.split('|').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0)
}

fn ack_type(ack: &str) -> &str {
    ack.split('|').next().unwrap_or("")
}

#[derive(Deserialize)]
pub struct StreamDto {
    types: Vec<String>,
    #[serde(default)]
    reset: bool,
}

/// Buffers JSON lines and hands them to the HTTP body in chunks. Stops when the client is gone.
struct Out {
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    buf: Vec<u8>,
}

struct ClientGone;

impl Out {
    fn line(&mut self, kind: &str, data: Value, ack: String) -> Result<(), ClientGone> {
        serde_json::to_writer(&mut self.buf, &json!({ "type": kind, "data": data, "ack": ack })).expect("json");
        self.buf.push(b'\n');
        if self.buf.len() >= CHUNK {
            self.flush()?;
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<(), ClientGone> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = Bytes::from(std::mem::take(&mut self.buf));
        self.tx.blocking_send(Ok(chunk)).map_err(|_| ClientGone)
    }
}

pub async fn stream(State(st): State<AppState>, auth: Auth, Json(dto): Json<StreamDto>) -> Response {
    let (tx, rx) = mpsc::channel(4);
    let db_path = st.cfg.db_path.clone();
    tokio::task::spawn_blocking(move || {
        let mut out = Out { tx, buf: Vec::with_capacity(CHUNK + 4096) };
        let res = db::open(&db_path).map_err(|e| e.to_string()).and_then(|mut c| produce(&mut c, &auth, &dto, &mut out).map_err(|e| e.to_string()));
        match res {
            Ok(Ok(())) => {
                let _ = out.flush();
            }
            Ok(Err(ClientGone)) => tracing::debug!("sync: client closed the connection"),
            Err(e) => {
                // Headers are already sent: cutting the stream makes the app retry later.
                tracing::error!("sync stream failed: {e}");
                let _ = out.tx.blocking_send(Err(std::io::Error::other(e)));
            }
        }
    });
    (StatusCode::OK, [(CONTENT_TYPE, "application/jsonlines+json")], Body::from_stream(ReceiverStream::new(rx))).into_response()
}

fn produce(c: &mut Connection, auth: &Auth, dto: &StreamDto, out: &mut Out) -> rusqlite::Result<Result<(), ClientGone>> {
    let sid = &auth.session_id;
    if dto.reset {
        c.execute("DELETE FROM sync_checkpoints WHERE session_id = ?1", [sid])?;
    }
    let checkpoints: HashMap<String, i64> = {
        let mut s = c.prepare("SELECT type, ack FROM sync_checkpoints WHERE session_id = ?1")?;
        let rows = s.query_map([sid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.filter_map(Result::ok).map(|(t, a)| (t, ack_update_id(&a))).collect()
    };
    let cp = |t: &str| checkpoints.get(t).copied().unwrap_or(0);
    // Snapshot: rows committed after this point wait for the next sync.
    let now = db::current_update_id(c)?;
    let uid = &auth.user.id;
    let wants = |t: &str| dto.types.iter().any(|x| x == t);

    macro_rules! send {
        ($kind:expr, $data:expr, $id:expr) => {
            if let Err(g) = out.line($kind, $data, ack($kind, $id)) {
                return Ok(Err(g));
            }
        };
    }

    // Same order as the Immich server: users, then assets, then EXIF.
    // (The v3.4.2 app asks AuthUsersV2 because the server reports 3.4.2; V1 is not answered.)
    if wants("AuthUsersV2") {
        let user = c.query_row(&format!("SELECT {USER_COLS} FROM users WHERE id = ?1"), [uid], db::user_from_row)?;
        if user.update_id > cp("AuthUserV2") && user.update_id <= now {
            let usage: i64 = c.query_row("SELECT COALESCE(SUM(size), 0) FROM assets WHERE owner_id = ?1", [uid], |r| r.get(0))?;
            send!("AuthUserV2", dto::sync_auth_user_v2(&user, usage), user.update_id);
        }
    }
    if wants("UsersV1") {
        let mut s = c.prepare(&format!("SELECT {USER_COLS} FROM users WHERE update_id > ?1 AND update_id <= ?2 ORDER BY update_id"))?;
        let users = s.query_map([cp("UserV1"), now], db::user_from_row)?.collect::<Result<Vec<_>, _>>()?;
        for u in users {
            send!("UserV1", dto::sync_user_v1(&u), u.update_id);
        }
    }
    if wants("AssetsV2") {
        let mut s = c.prepare("SELECT asset_id, update_id FROM asset_deletes WHERE owner_id = ?1 AND update_id > ?2 AND update_id <= ?3 ORDER BY update_id")?;
        let mut rows = s.query(rusqlite::params![uid, cp("AssetDeleteV1"), now])?;
        while let Some(r) = rows.next()? {
            let (id, u): (String, i64) = (r.get(0)?, r.get(1)?);
            send!("AssetDeleteV1", json!({ "assetId": id }), u);
        }
        drop(rows);
        let mut s = c.prepare(&format!(
            "SELECT {ASSET_COLS} FROM assets WHERE owner_id = ?1 AND update_id > ?2 AND update_id <= ?3 ORDER BY update_id"
        ))?;
        let mut rows = s.query(rusqlite::params![uid, cp("AssetV2"), now])?;
        while let Some(r) = rows.next()? {
            let a = asset_from_row(r)?;
            send!("AssetV2", dto::sync_asset_v2(&a), a.update_id);
        }
    }
    if wants("AssetExifsV1") {
        let mut s = c.prepare(
            "SELECT e.asset_id, e.data, e.update_id FROM exif e JOIN assets a ON a.id = e.asset_id
             WHERE a.owner_id = ?1 AND e.update_id > ?2 AND e.update_id <= ?3 ORDER BY e.update_id",
        )?;
        let mut rows = s.query(rusqlite::params![uid, cp("AssetExifV1"), now])?;
        while let Some(r) = rows.next()? {
            let (id, data, u): (String, String, i64) = (r.get(0)?, r.get(1)?, r.get(2)?);
            send!("AssetExifV1", dto::sync_exif_v1(&id, &data), u);
        }
    }
    send!("SyncCompleteV1", json!({}), now);
    Ok(Ok(()))
}

#[derive(Deserialize)]
pub struct AckSetDto {
    acks: Vec<String>,
}

pub async fn ack_set(State(st): State<AppState>, auth: Auth, Json(dto): Json<AckSetDto>) -> ApiResult<StatusCode> {
    let sid = auth.session_id.clone();
    st.db(move |c| {
        let tx = c.transaction()?;
        if dto.acks.iter().any(|a| ack_type(a) == "SyncResetV1") {
            tx.execute("DELETE FROM sync_checkpoints WHERE session_id = ?1", [&sid])?;
        } else {
            for a in &dto.acks {
                let t = ack_type(a);
                if t.is_empty() {
                    continue;
                }
                tx.execute(
                    "INSERT INTO sync_checkpoints(session_id, type, ack) VALUES(?1, ?2, ?3)
                     ON CONFLICT(session_id, type) DO UPDATE SET ack = excluded.ack",
                    rusqlite::params![sid, t, a],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn ack_get(State(st): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    let sid = auth.session_id.clone();
    let rows = st
        .db(move |c| {
            let mut s = c.prepare("SELECT type, ack FROM sync_checkpoints WHERE session_id = ?1 ORDER BY type")?;
            let v = s
                .query_map([sid], |r| Ok(json!({ "type": r.get::<_, String>(0)?, "ack": r.get::<_, String>(1)? })))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })
        .await?;
    Ok(Json(Value::Array(rows)))
}

#[derive(Deserialize, Default)]
pub struct AckDeleteDto {
    types: Option<Vec<String>>,
}

pub async fn ack_delete(State(st): State<AppState>, auth: Auth, body: Option<Json<AckDeleteDto>>) -> ApiResult<StatusCode> {
    let sid = auth.session_id.clone();
    let types = body.map(|Json(d)| d).unwrap_or_default().types;
    st.db(move |c| {
        match types {
            Some(ts) => {
                for t in ts {
                    c.execute("DELETE FROM sync_checkpoints WHERE session_id = ?1 AND type = ?2", [&sid, &t])?;
                }
            }
            None => {
                c.execute("DELETE FROM sync_checkpoints WHERE session_id = ?1", [&sid])?;
            }
        }
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acks() {
        assert_eq!(ack("AssetV2", 42), "AssetV2|00000000000000000042");
        assert_eq!(ack_update_id("AssetV2|00000000000000000042"), 42);
        assert_eq!(ack_update_id("AssetV2|0190f6a4-uuid"), 0);
        assert_eq!(ack_update_id("garbage"), 0);
        assert_eq!(ack_type("SyncResetV1|reset"), "SyncResetV1");
    }
}
