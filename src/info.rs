//! Server info and current user endpoints (what the app reads at startup).

use crate::auth::Auth;
use crate::dto;
use crate::error::ApiResult;
use crate::state::AppState;
use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

/// The app picks its sync request types from the server version: 3.4.2 = the version of the forked app.
pub const IMMICH_VERSION: (u32, u32, u32) = (3, 4, 2);

pub async fn ping() -> Json<Value> {
    Json(json!({ "res": "pong" }))
}

pub async fn version() -> Json<Value> {
    let (major, minor, patch) = IMMICH_VERSION;
    // featureLevel only enables passkeys in the app: 0 = off.
    Json(json!({ "major": major, "minor": minor, "patch": patch, "prerelease": null, "featureLevel": 0 }))
}

pub async fn features() -> Json<Value> {
    Json(json!({
        "passwordLogin": true,
        "trash": true,
        "smartSearch": false,
        "facialRecognition": false,
        "duplicateDetection": false,
        "map": false,
        "reverseGeocoding": false,
        "importFaces": false,
        "sidecar": false,
        "search": false,
        "oauth": false,
        "oauthAutoLaunch": false,
        "ocr": false,
        "configFile": false,
        "email": false,
        "backups": false,
        "realtimeTranscoding": false,
    }))
}

pub async fn config(State(st): State<AppState>) -> Json<Value> {
    Json(json!({
        "loginPageMessage": "",
        "trashDays": st.cfg.trash_days,
        "userDeleteDelay": 7,
        "oauthButtonText": "",
        "isInitialized": true,
        "isOnboarded": true,
        "externalDomain": "",
        "publicUsers": false,
        "mapDarkStyleUrl": "",
        "mapLightStyleUrl": "",
        "minFaces": 3,
        "maintenanceMode": false,
    }))
}

fn human_size(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", units[i]) }
}

/// (size, available, used) of the filesystem holding `path`.
fn disk_usage(path: &std::path::Path) -> std::io::Result<(u64, u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let frsize = s.f_frsize as u64;
    let size = s.f_blocks as u64 * frsize;
    let avail = s.f_bavail as u64 * frsize;
    let used = size.saturating_sub(s.f_bfree as u64 * frsize);
    Ok((size, avail, used))
}

pub async fn storage(State(st): State<AppState>, _auth: Auth) -> ApiResult<Json<Value>> {
    let (size, avail, used) = disk_usage(&st.cfg.photo_root)?;
    let pct = if size == 0 { 0.0 } else { (used as f64 / size as f64 * 10000.0).round() / 100.0 };
    Ok(Json(json!({
        "diskSize": human_size(size),
        "diskUse": human_size(used),
        "diskAvailable": human_size(avail),
        "diskSizeRaw": size,
        "diskUseRaw": used,
        "diskAvailableRaw": avail,
        "diskUsagePercentage": pct,
    })))
}

pub async fn me(State(st): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    let uid = auth.user.id.clone();
    let usage: i64 = st
        .db(move |c| Ok(c.query_row("SELECT COALESCE(SUM(size), 0) FROM assets WHERE owner_id = ?1", [uid], |r| r.get(0))?))
        .await?;
    Ok(Json(dto::user_admin(&auth.user, usage)))
}

/// Server defaults (server/src/utils/preferences.ts), with the features PhotoPI lacks turned off.
pub async fn preferences(_auth: Auth) -> Json<Value> {
    Json(json!({
        "albums": { "defaultAssetOrder": "desc" },
        "folders": { "enabled": false, "sidebarWeb": false },
        "memories": { "enabled": false, "duration": 5, "sidebarWeb": false },
        "people": { "enabled": false, "sidebarWeb": false, "minimumFaces": 3, "updateStrategy": "everyone" },
        "sharedLinks": { "enabled": false, "sidebarWeb": false },
        "ratings": { "enabled": false },
        "tags": { "enabled": false, "sidebarWeb": false },
        "emailNotifications": { "enabled": false, "albumInvite": false, "albumUpdate": false },
        "download": { "archiveSize": 4u64 * 1024 * 1024 * 1024, "includeEmbeddedVideos": false },
        "purchase": { "showSupportBadge": false, "hideBuyButtonUntil": "2100-01-01T00:00:00.000Z" },
        "cast": { "gCastEnabled": false },
        "recentlyAdded": { "sidebarWeb": false },
    }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn human_size() {
        assert_eq!(super::human_size(512), "512 B");
        assert_eq!(super::human_size(1536), "1.5 KiB");
        assert_eq!(super::human_size(1_000_000_000_000), "931.3 GiB");
    }
}
