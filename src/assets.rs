//! Upload, read, update, trash and delete assets.
//!
//! Files go to `PHOTO_ROOT/<storage_label>/<YYYY>/<MM>/<original name>` (date = when the photo was taken,
//! wall clock). An upload is streamed to `PHOTO_ROOT/.upload-tmp` (same disk, so the final move is a
//! rename) while its SHA-1 is computed; a file already stored for the user is answered as `duplicate`.

use crate::auth::Auth;
use crate::db::{self, User};
use crate::dto::{self, asset_by_id, Asset};
use crate::error::{ApiError, ApiResult};
use crate::media::{self, Kind};
use crate::state::{AppState, Config};
use crate::thumbs::{Size, Thumbnailer};
use axum::body::Body;
use axum::extract::{Multipart, Path, Query, Request, State};
use axum::http::header::CACHE_CONTROL;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{Datelike, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tower::ServiceExt;
use tower_http::services::ServeFile;

// ---------- upload ----------

/// Deletes the file when dropped, unless disarmed (moved to its final place).
struct TempFile {
    path: PathBuf,
    armed: bool,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

struct Received {
    tmp: TempFile,
    client_name: String,
    content_type: Option<String>,
    checksum: Vec<u8>,
    size: u64,
}

enum Ingested {
    Created(String),
    Duplicate(String),
}

pub async fn upload(State(st): State<AppState>, auth: Auth, mut mp: Multipart) -> ApiResult<Response> {
    let tmp_dir = st.tmp_dir();
    tokio::fs::create_dir_all(&tmp_dir).await?;
    let mut fields: HashMap<String, String> = HashMap::new();
    let mut received: Option<Received> = None;

    while let Some(mut field) = mp.next_field().await.map_err(|e| ApiError::bad_request(format!("multipart: {e}")))? {
        let name = field.name().unwrap_or("").to_string();
        if name != "assetData" {
            let value = field.text().await.map_err(|e| ApiError::bad_request(format!("field {name}: {e}")))?;
            fields.insert(name, value);
            continue;
        }
        let mut tmp = TempFile { path: tmp_dir.join(format!("{}.part", uuid::Uuid::new_v4())), armed: true };
        let client_name = field.file_name().unwrap_or("upload").to_string();
        let content_type = field.content_type().map(str::to_string);
        let file = tokio::fs::File::create(&tmp.path).await?;
        let mut out = tokio::io::BufWriter::with_capacity(512 * 1024, file);
        let mut hasher = Sha1::new();
        let mut size = 0u64;
        while let Some(chunk) = field.chunk().await.map_err(|e| ApiError::bad_request(format!("upload interrupted: {e}")))? {
            hasher.update(&chunk);
            size += chunk.len() as u64;
            out.write_all(&chunk).await?;
        }
        out.flush().await?;
        // On disk for real before we answer: the phone may free its own copy afterwards.
        out.into_inner().sync_all().await?;
        tmp.armed = true;
        received = Some(Received { tmp, client_name, content_type, checksum: hasher.finalize().to_vec(), size });
    }

    let rec = received.ok_or_else(|| ApiError::bad_request("assetData is missing"))?;
    if rec.size == 0 {
        return Err(ApiError::bad_request("Empty file"));
    }
    let file_name = fields.get("filename").filter(|s| !s.trim().is_empty()).cloned().unwrap_or_else(|| rec.client_name.clone());
    let (kind, mime) = media::classify(&file_name, rec.content_type.as_deref()).ok_or_else(|| ApiError::bad_request(format!("Unsupported file type: {file_name}")))?;

    let input = IngestInput {
        file_name,
        kind,
        mime,
        app_created: fields.get("fileCreatedAt").and_then(|s| media::parse_app_date(s)),
        app_modified: fields.get("fileModifiedAt").and_then(|s| media::parse_app_date(s)),
        is_favorite: fields.get("isFavorite").is_some_and(|v| v == "true"),
        device_asset_id: fields.get("deviceAssetId").cloned(),
        device_id: fields.get("deviceId").cloned(),
    };
    let cfg = st.cfg.clone();
    let user = auth.user.clone();
    let result = tokio::task::spawn_blocking(move || ingest(&cfg, &user, rec, input))
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("ingest task: {e}")))??;

    Ok(match result {
        Ingested::Created(id) => {
            st.thumbs.enqueue(id.clone());
            (StatusCode::CREATED, Json(json!({ "id": id, "status": "created" }))).into_response()
        }
        Ingested::Duplicate(id) => (StatusCode::OK, Json(json!({ "id": id, "status": "duplicate" }))).into_response(),
    })
}

struct IngestInput {
    file_name: String,
    kind: Kind,
    mime: String,
    app_created: Option<chrono::DateTime<Utc>>,
    app_modified: Option<chrono::DateTime<Utc>>,
    is_favorite: bool,
    device_asset_id: Option<String>,
    device_id: Option<String>,
}

/// Windows/NTFS-safe file name, extension kept, at most ~180 bytes.
pub fn sanitize_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mut s: String = base
        .chars()
        .map(|c| if c.is_control() || matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    s = s.trim_matches(|c: char| c == '.' || c.is_whitespace()).to_string();
    if s.is_empty() {
        s = "file".into();
    }
    if s.len() > 180 {
        let ext = FsPath::new(&s).extension().and_then(|e| e.to_str()).map(|e| format!(".{e}")).unwrap_or_default();
        let mut stem: String = s.chars().take(160).collect();
        while stem.len() > 160 {
            stem.pop();
        }
        s = format!("{stem}{ext}");
    }
    s
}

/// `name.jpg`, then `name+1.jpg`, `name+2.jpg`... (first free one).
fn unique_name(dir: &FsPath, name: &str) -> String {
    if !dir.join(name).exists() {
        return name.to_string();
    }
    let p = FsPath::new(name);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = p.extension().and_then(|e| e.to_str()).map(|e| format!(".{e}")).unwrap_or_default();
    (1..)
        .map(|n| format!("{stem}+{n}{ext}"))
        .find(|candidate| !dir.join(candidate).exists())
        .expect("unbounded")
}

/// Serialises "pick a free name + rename": two uploads with the same name must not overwrite each other.
static PLACE_LOCK: Mutex<()> = Mutex::new(());

fn existing_by_checksum(conn: &Connection, owner: &str, checksum: &[u8]) -> rusqlite::Result<Option<(String, Option<i64>)>> {
    conn.query_row(
        "SELECT id, deleted_at FROM assets WHERE owner_id = ?1 AND checksum = ?2",
        params![owner, checksum],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

fn ingest(cfg: &Config, user: &User, mut rec: Received, input: IngestInput) -> ApiResult<Ingested> {
    let mut conn = db::open(&cfg.db_path)?;

    if let Some((id, deleted_at)) = existing_by_checksum(&conn, &user.id, &rec.checksum)? {
        if deleted_at.is_some() {
            // Uploading again something that is in the trash: take it out of the trash.
            let tx = db::write_tx(&mut conn)?;
            let uid = db::next_update_id(&tx)?;
            tx.execute("UPDATE assets SET deleted_at = NULL, updated_at = ?1, update_id = ?2 WHERE id = ?3", params![db::now_ms(), uid, id])?;
            tx.commit()?;
        }
        return Ok(Ingested::Duplicate(id));
    }

    let meta = media::read_meta(&rec.tmp.path, input.kind);
    let (taken_utc, taken_local, tz) = media::resolve_dates(&meta, input.app_created);
    let mut exif = meta.exif.clone();
    exif.insert("dateTimeOriginal".into(), json!(dto::iso(taken_utc.timestamp_millis())));
    if let Some(tz) = tz {
        exif.insert("timeZone".into(), json!(tz));
    }

    let rel_dir = format!("{}/{:04}/{:02}", user.storage_label, taken_local.year(), taken_local.month());
    let abs_dir = cfg.photo_root.join(&rel_dir);
    std::fs::create_dir_all(&abs_dir)?;
    let wanted = sanitize_file_name(&input.file_name);
    let (rel_path, abs_path) = {
        let _guard = PLACE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let name = unique_name(&abs_dir, &wanted);
        let abs = abs_dir.join(&name);
        std::fs::rename(&rec.tmp.path, &abs)?;
        rec.tmp.armed = false;
        (format!("{rel_dir}/{name}"), abs)
    };

    let id = uuid::Uuid::new_v4().to_string();
    let now = db::now_ms();
    let created = taken_utc.timestamp_millis();
    let modified = input.app_modified.map(|d| d.timestamp_millis()).unwrap_or(created);
    let insert = (|| -> rusqlite::Result<()> {
        let tx = db::write_tx(&mut conn)?;
        let uid_asset = db::next_update_id(&tx)?;
        tx.execute(
            "INSERT INTO assets(id, owner_id, path, original_file_name, checksum, type, mime, size, width, height, duration_ms,
               file_created_at, file_modified_at, local_date_time, created_at, updated_at, is_favorite, device_asset_id, device_id, update_id)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?15, ?16, ?17, ?18, ?19)",
            params![
                id,
                user.id,
                rel_path,
                input.file_name,
                rec.checksum,
                input.kind.as_str(),
                input.mime,
                rec.size as i64,
                meta.width,
                meta.height,
                meta.duration_ms,
                created,
                modified,
                media::fmt_local(taken_local),
                now,
                input.is_favorite,
                input.device_asset_id,
                input.device_id,
                uid_asset
            ],
        )?;
        let uid_exif = db::next_update_id(&tx)?;
        tx.execute(
            "INSERT INTO exif(asset_id, data, update_id) VALUES(?1, ?2, ?3)",
            params![id, serde_json::Value::Object(exif).to_string(), uid_exif],
        )?;
        tx.commit()
    })();

    match insert {
        Ok(()) => {
            tracing::info!("upload: {} -> {} ({} bytes)", input.file_name, rel_path, rec.size);
            Ok(Ingested::Created(id))
        }
        Err(e) => {
            let _ = std::fs::remove_file(&abs_path);
            // Same file uploaded twice at the same time: the other request won.
            if let Some((existing, _)) = existing_by_checksum(&conn, &user.id, &rec.checksum)? {
                return Ok(Ingested::Duplicate(existing));
            }
            Err(e.into())
        }
    }
}

// ---------- read ----------

async fn load(st: &AppState, auth: &Auth, id: String) -> ApiResult<Asset> {
    let owner = auth.user.id.clone();
    st.db(move |c| asset_by_id(c, &id, &owner)?.ok_or_else(ApiError::not_found)).await
}

async fn serve_file(path: PathBuf, req: Request, cache: bool) -> ApiResult<Response> {
    if !path.exists() {
        return Err(ApiError::not_found());
    }
    let res = match ServeFile::new(path).oneshot(req).await {
        Ok(r) => r,
        Err(never) => match never {},
    };
    let mut res = res.map(Body::new);
    if cache && res.status().is_success() {
        res.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("private, max-age=86400, no-transform"));
    }
    Ok(res)
}

pub async fn info(State(st): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let owner = auth.user.id.clone();
    let (asset, exif) = st
        .db(move |c| {
            let a = asset_by_id(c, &id, &owner)?.ok_or_else(ApiError::not_found)?;
            let exif: Option<String> = c.query_row("SELECT data FROM exif WHERE asset_id = ?1", [&a.id], |r| r.get(0)).optional()?;
            Ok((a, exif))
        })
        .await?;
    Ok(Json(dto::asset_response(&asset, &auth.user, exif.as_deref())))
}

#[derive(Deserialize)]
pub struct ThumbQuery {
    size: Option<String>,
}

/// Formats every phone can display as they are.
fn browser_friendly(mime: &str) -> bool {
    matches!(mime, "image/jpeg" | "image/png" | "image/webp" | "image/gif")
}

pub async fn thumbnail(State(st): State<AppState>, auth: Auth, Path(id): Path<String>, Query(q): Query<ThumbQuery>, req: Request) -> ApiResult<Response> {
    let asset = load(&st, &auth, id).await?;
    let size = match q.size.as_deref() {
        Some("preview") => Size::Preview,
        Some("fullsize") | Some("original") => {
            if asset.kind == "IMAGE" && browser_friendly(&asset.mime) {
                return serve_file(st.cfg.photo_root.join(&asset.path), req, true).await;
            }
            Size::Preview
        }
        _ => Size::Thumbnail,
    };
    if let Err(e) = st.thumbs.ensure(&asset).await {
        tracing::warn!("thumbnail {} ({}): {e:#}", asset.id, asset.path);
        return Err(ApiError::not_found());
    }
    serve_file(st.thumbs.path(&asset.id, size), req, true).await
}

pub async fn original(State(st): State<AppState>, auth: Auth, Path(id): Path<String>, req: Request) -> ApiResult<Response> {
    let asset = load(&st, &auth, id).await?;
    serve_file(st.cfg.photo_root.join(&asset.path), req, false).await
}

/// No transcoding: the phone plays the original (it recorded it).
pub async fn video_playback(State(st): State<AppState>, auth: Auth, Path(id): Path<String>, req: Request) -> ApiResult<Response> {
    let asset = load(&st, &auth, id).await?;
    if asset.kind != "VIDEO" {
        return Err(ApiError::bad_request("Not a video"));
    }
    serve_file(st.cfg.photo_root.join(&asset.path), req, false).await
}

// ---------- update ----------

fn check_visibility(v: &Option<String>) -> ApiResult<()> {
    match v.as_deref() {
        None | Some("timeline") | Some("archive") => Ok(()),
        Some(other) => Err(ApiError::bad_request(format!("Visibility '{other}' is not supported by PhotoPI"))),
    }
}

/// Applies favorite / visibility / description to one asset (inside a write transaction).
fn apply_update(tx: &Connection, owner: &str, id: &str, fav: Option<bool>, vis: &Option<String>, description: &Option<String>) -> rusqlite::Result<bool> {
    let now = db::now_ms();
    if fav.is_none() && vis.is_none() && description.is_none() {
        return Ok(false);
    }
    let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM assets WHERE id = ?1 AND owner_id = ?2)", [id, owner], |r| r.get(0))?;
    if !exists {
        return Ok(false);
    }
    if fav.is_some() || vis.is_some() {
        let uid = db::next_update_id(tx)?;
        tx.execute(
            "UPDATE assets SET is_favorite = COALESCE(?1, is_favorite), visibility = COALESCE(?2, visibility), updated_at = ?3, update_id = ?4 WHERE id = ?5",
            params![fav, vis, now, uid, id],
        )?;
    }
    if let Some(d) = description {
        let data: String = tx.query_row("SELECT data FROM exif WHERE asset_id = ?1", [id], |r| r.get(0)).unwrap_or_else(|_| "{}".into());
        let mut obj: serde_json::Map<String, Value> = serde_json::from_str(&data).unwrap_or_default();
        obj.insert("description".into(), json!(d));
        let uid = db::next_update_id(tx)?;
        tx.execute(
            "INSERT INTO exif(asset_id, data, update_id) VALUES(?1, ?2, ?3) ON CONFLICT(asset_id) DO UPDATE SET data = excluded.data, update_id = excluded.update_id",
            params![id, Value::Object(obj).to_string(), uid],
        )?;
    }
    Ok(true)
}

/// Fields the app can send that PhotoPI does not handle yet: refuse rather than pretend.
fn reject_unsupported(v: &Value) -> ApiResult<()> {
    for k in ["dateTimeOriginal", "dateTimeRelative", "latitude", "longitude", "rating", "livePhotoVideoId", "timeZone", "duplicateId"] {
        if v.get(k).is_some_and(|x| !x.is_null()) {
            return Err(ApiError::new(StatusCode::NOT_IMPLEMENTED, format!("Changing '{k}' is not supported by PhotoPI")));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateFields {
    is_favorite: Option<bool>,
    visibility: Option<String>,
    description: Option<String>,
}

pub async fn update_one(State(st): State<AppState>, auth: Auth, Path(id): Path<String>, Json(body): Json<Value>) -> ApiResult<Json<Value>> {
    reject_unsupported(&body)?;
    let f: UpdateFields = serde_json::from_value(body).map_err(|e| ApiError::bad_request(e.to_string()))?;
    check_visibility(&f.visibility)?;
    let owner = auth.user.id.clone();
    let (asset, exif) = st
        .db(move |c| {
            let tx = db::write_tx(c)?;
            apply_update(&tx, &owner, &id, f.is_favorite, &f.visibility, &f.description)?;
            tx.commit()?;
            let a = asset_by_id(c, &id, &owner)?.ok_or_else(ApiError::not_found)?;
            let exif: Option<String> = c.query_row("SELECT data FROM exif WHERE asset_id = ?1", [&a.id], |r| r.get(0)).optional()?;
            Ok((a, exif))
        })
        .await?;
    Ok(Json(dto::asset_response(&asset, &auth.user, exif.as_deref())))
}

pub async fn update_bulk(State(st): State<AppState>, auth: Auth, Json(body): Json<Value>) -> ApiResult<StatusCode> {
    reject_unsupported(&body)?;
    let ids: Vec<String> = serde_json::from_value(body.get("ids").cloned().unwrap_or(Value::Null)).map_err(|_| ApiError::bad_request("ids is required"))?;
    let f: UpdateFields = serde_json::from_value(body).map_err(|e| ApiError::bad_request(e.to_string()))?;
    check_visibility(&f.visibility)?;
    let owner = auth.user.id.clone();
    st.db(move |c| {
        let tx = db::write_tx(c)?;
        for id in &ids {
            apply_update(&tx, &owner, id, f.is_favorite, &f.visibility, &f.description)?;
        }
        tx.commit()?;
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------- trash and delete ----------

/// Permanently deletes assets (rows first, then files: a failure never loses a file by mistake).
pub fn hard_delete(conn: &mut Connection, photo_root: &FsPath, thumbs: &Thumbnailer, owner: Option<&str>, ids: &[String]) -> rusqlite::Result<usize> {
    let mut removed: Vec<(String, String)> = Vec::new();
    let tx = db::write_tx(conn)?;
    for id in ids {
        let row: Option<(String, String)> = tx
            .query_row("SELECT path, owner_id FROM assets WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let Some((path, asset_owner)) = row else { continue };
        if owner.is_some_and(|o| o != asset_owner) {
            continue;
        }
        tx.execute("DELETE FROM assets WHERE id = ?1", [id])?;
        let uid = db::next_update_id(&tx)?;
        tx.execute("INSERT INTO asset_deletes(asset_id, owner_id, update_id) VALUES(?1, ?2, ?3)", params![id, asset_owner, uid])?;
        removed.push((id.clone(), path));
    }
    tx.commit()?;
    for (id, path) in &removed {
        match std::fs::remove_file(photo_root.join(path)) {
            Ok(()) => tracing::info!("deleted {path}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("could not delete {path}: {e}"),
        }
        thumbs.remove(id);
    }
    Ok(removed.len())
}

fn trash(conn: &mut Connection, owner: &str, ids: &[String]) -> rusqlite::Result<usize> {
    let tx = db::write_tx(conn)?;
    let now = db::now_ms();
    let mut n = 0;
    for id in ids {
        let in_timeline: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM assets WHERE id = ?1 AND owner_id = ?2 AND deleted_at IS NULL)",
            [id, owner],
            |r| r.get(0),
        )?;
        if in_timeline {
            let uid = db::next_update_id(&tx)?;
            n += tx.execute("UPDATE assets SET deleted_at = ?1, updated_at = ?1, update_id = ?2 WHERE id = ?3", params![now, uid, id])?;
        }
    }
    tx.commit()?;
    Ok(n)
}

fn restore(conn: &mut Connection, owner: &str, ids: Option<&[String]>) -> rusqlite::Result<usize> {
    let ids: Vec<String> = match ids {
        Some(i) => i.to_vec(),
        None => {
            let mut st = conn.prepare("SELECT id FROM assets WHERE owner_id = ?1 AND deleted_at IS NOT NULL")?;
            let v = st.query_map([owner], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            v
        }
    };
    let tx = db::write_tx(conn)?;
    let now = db::now_ms();
    let mut n = 0;
    for id in &ids {
        let trashed: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM assets WHERE id = ?1 AND owner_id = ?2 AND deleted_at IS NOT NULL)",
            [id, owner],
            |r| r.get(0),
        )?;
        if trashed {
            let uid = db::next_update_id(&tx)?;
            n += tx.execute("UPDATE assets SET deleted_at = NULL, updated_at = ?1, update_id = ?2 WHERE id = ?3", params![now, uid, id])?;
        }
    }
    tx.commit()?;
    Ok(n)
}

#[derive(Deserialize)]
pub struct BulkDelete {
    ids: Vec<String>,
    #[serde(default)]
    force: bool,
}

/// Without `force`: to the trash (emptied after `trashDays`). With `force`: gone for good.
pub async fn delete(State(st): State<AppState>, auth: Auth, Json(dto): Json<BulkDelete>) -> ApiResult<StatusCode> {
    let owner = auth.user.id.clone();
    let root = st.cfg.photo_root.clone();
    let thumbs = st.thumbs.clone();
    let n = st
        .db(move |c| {
            Ok(if dto.force { hard_delete(c, &root, &thumbs, Some(&owner), &dto.ids)? } else { trash(c, &owner, &dto.ids)? })
        })
        .await?;
    tracing::info!("delete: {n} asset(s)");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetIds {
    asset_ids: Vec<String>,
}

pub async fn trash_restore_assets(State(st): State<AppState>, auth: Auth, Json(dto): Json<AssetIds>) -> ApiResult<Json<Value>> {
    let owner = auth.user.id.clone();
    let n = st.db(move |c| Ok(restore(c, &owner, Some(&dto.asset_ids))?)).await?;
    Ok(Json(json!({ "count": n })))
}

pub async fn trash_restore_all(State(st): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    let owner = auth.user.id.clone();
    let n = st.db(move |c| Ok(restore(c, &owner, None)?)).await?;
    Ok(Json(json!({ "count": n })))
}

pub async fn trash_empty(State(st): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    let owner = auth.user.id.clone();
    let root = st.cfg.photo_root.clone();
    let thumbs = st.thumbs.clone();
    let n = st
        .db(move |c| {
            let mut s = c.prepare("SELECT id FROM assets WHERE owner_id = ?1 AND deleted_at IS NOT NULL")?;
            let ids = s.query_map([&owner], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            drop(s);
            Ok(hard_delete(c, &root, &thumbs, Some(&owner), &ids)?)
        })
        .await?;
    Ok(Json(json!({ "count": n })))
}

/// Every 6 hours: permanently deletes what has been in the trash longer than `trash_days`.
pub async fn purge_loop(cfg: Arc<Config>, thumbs: Arc<Thumbnailer>) {
    loop {
        let (cfg2, thumbs2) = (cfg.clone(), thumbs.clone());
        let res = tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
            let mut c = db::open(&cfg2.db_path)?;
            let limit = db::now_ms() - cfg2.trash_days * 86_400_000;
            let mut s = c.prepare("SELECT id FROM assets WHERE deleted_at IS NOT NULL AND deleted_at < ?1")?;
            let ids = s.query_map([limit], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            drop(s);
            Ok(hard_delete(&mut c, &cfg2.photo_root, &thumbs2, None, &ids)?)
        })
        .await;
        match res {
            Ok(Ok(0)) => {}
            Ok(Ok(n)) => tracing::info!("trash: {n} asset(s) older than {} days deleted", cfg.trash_days),
            other => tracing::warn!("trash purge failed: {other:?}"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names() {
        assert_eq!(sanitize_file_name("PXL_20240501_120322123.jpg"), "PXL_20240501_120322123.jpg");
        assert_eq!(sanitize_file_name("a:b*c?.jpg"), "a_b_c_.jpg");
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("..."), "file");
        let long = format!("{}.jpg", "é".repeat(200));
        let s = sanitize_file_name(&long);
        assert!(s.len() <= 164 && s.ends_with(".jpg"), "{s}");
    }

    #[test]
    fn unique_names() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(unique_name(dir.path(), "a.jpg"), "a.jpg");
        std::fs::write(dir.path().join("a.jpg"), b"x").unwrap();
        assert_eq!(unique_name(dir.path(), "a.jpg"), "a+1.jpg");
        std::fs::write(dir.path().join("a+1.jpg"), b"x").unwrap();
        assert_eq!(unique_name(dir.path(), "a.jpg"), "a+2.jpg");
    }
}
