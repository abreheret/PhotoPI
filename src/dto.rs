//! Asset rows and the JSON shapes the Immich app expects (field names from open-api/immich-openapi-specs.json).

use crate::db::User;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chrono::{DateTime, SecondsFormat};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Map, Value};

/// Unix ms -> "2024-05-01T12:03:22.000Z".
pub fn iso(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms).unwrap_or_default().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Stored wall-clock time -> Immich `localDateTime` (wall clock written as if it were UTC).
pub fn local_iso(local: &str) -> String {
    format!("{local}Z")
}

pub fn user_admin(u: &User, usage: i64) -> Value {
    json!({
        "id": u.id,
        "email": u.email,
        "name": u.name,
        "isAdmin": u.is_admin,
        "avatarColor": "primary",
        "profileImagePath": "",
        "profileChangedAt": iso(u.profile_changed_at),
        "createdAt": iso(u.created_at),
        "updatedAt": iso(u.updated_at),
        "deletedAt": null,
        "oauthId": "",
        "shouldChangePassword": false,
        "status": "active",
        "storageLabel": u.storage_label,
        "quotaSizeInBytes": null,
        "quotaUsageInBytes": usage,
        "license": null,
        "clusterGroupId": u.id,
    })
}

pub fn sync_auth_user_v2(u: &User, usage: i64) -> Value {
    json!({
        "id": u.id,
        "name": u.name,
        "email": u.email,
        "isAdmin": u.is_admin,
        "avatarColor": null,
        "deletedAt": null,
        "hasProfileImage": false,
        "profileChangedAt": iso(u.profile_changed_at),
        "oauthId": null,
        "pinCode": null,
        "storageLabel": u.storage_label,
        "quotaSizeInBytes": null,
        "quotaUsageInBytes": usage,
    })
}

pub fn sync_user_v1(u: &User) -> Value {
    json!({
        "id": u.id,
        "name": u.name,
        "email": u.email,
        "avatarColor": null,
        "deletedAt": null,
        "hasProfileImage": false,
        "profileChangedAt": iso(u.profile_changed_at),
    })
}

#[derive(Debug, Clone)]
pub struct Asset {
    pub id: String,
    pub owner_id: String,
    pub path: String,
    pub original_file_name: String,
    pub checksum: Vec<u8>,
    pub kind: String,
    pub mime: String,
    pub size: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration_ms: Option<i64>,
    pub file_created_at: i64,
    pub file_modified_at: i64,
    pub local_date_time: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
    pub is_favorite: bool,
    pub visibility: String,
    pub thumbhash: Option<Vec<u8>>,
    pub update_id: i64,
}

pub const ASSET_COLS: &str = "id, owner_id, path, original_file_name, checksum, type, mime, size, width, height, duration_ms, \
     file_created_at, file_modified_at, local_date_time, created_at, updated_at, deleted_at, is_favorite, visibility, thumbhash, update_id";

pub fn asset_from_row(r: &rusqlite::Row) -> rusqlite::Result<Asset> {
    Ok(Asset {
        id: r.get(0)?,
        owner_id: r.get(1)?,
        path: r.get(2)?,
        original_file_name: r.get(3)?,
        checksum: r.get(4)?,
        kind: r.get(5)?,
        mime: r.get(6)?,
        size: r.get(7)?,
        width: r.get(8)?,
        height: r.get(9)?,
        duration_ms: r.get(10)?,
        file_created_at: r.get(11)?,
        file_modified_at: r.get(12)?,
        local_date_time: r.get(13)?,
        created_at: r.get(14)?,
        updated_at: r.get(15)?,
        deleted_at: r.get(16)?,
        is_favorite: r.get(17)?,
        visibility: r.get(18)?,
        thumbhash: r.get(19)?,
        update_id: r.get(20)?,
    })
}

pub fn asset_by_id(conn: &Connection, id: &str, owner_id: &str) -> rusqlite::Result<Option<Asset>> {
    conn.query_row(
        &format!("SELECT {ASSET_COLS} FROM assets WHERE id = ?1 AND owner_id = ?2"),
        [id, owner_id],
        asset_from_row,
    )
    .optional()
}

pub fn sync_asset_v2(a: &Asset) -> Value {
    json!({
        "id": a.id,
        "ownerId": a.owner_id,
        "originalFileName": a.original_file_name,
        "checksum": B64.encode(&a.checksum),
        "type": a.kind,
        "visibility": a.visibility,
        "isFavorite": a.is_favorite,
        "isEdited": false,
        "createdAt": iso(a.created_at),
        "deletedAt": a.deleted_at.map(iso),
        "fileCreatedAt": iso(a.file_created_at),
        "fileModifiedAt": iso(a.file_modified_at),
        "localDateTime": local_iso(&a.local_date_time),
        "duration": a.duration_ms,
        "width": a.width,
        "height": a.height,
        "thumbhash": a.thumbhash.as_ref().map(|t| B64.encode(t)),
        "libraryId": null,
        "livePhotoVideoId": null,
        "stackId": null,
    })
}

/// SyncAssetExifV1 = assetId + every EXIF key (null when unknown).
pub fn sync_exif_v1(asset_id: &str, data: &str) -> Value {
    let mut obj = full_exif(data);
    obj.insert("assetId".into(), json!(asset_id));
    Value::Object(obj)
}

pub const EXIF_KEYS: [&str; 24] = [
    "description",
    "exifImageWidth",
    "exifImageHeight",
    "fileSizeInByte",
    "orientation",
    "dateTimeOriginal",
    "modifyDate",
    "timeZone",
    "latitude",
    "longitude",
    "projectionType",
    "city",
    "state",
    "country",
    "make",
    "model",
    "lensModel",
    "fNumber",
    "focalLength",
    "iso",
    "exposureTime",
    "profileDescription",
    "rating",
    "fps",
];

fn full_exif(data: &str) -> Map<String, Value> {
    let stored: Map<String, Value> = serde_json::from_str(data).unwrap_or_default();
    let mut obj = Map::new();
    for k in EXIF_KEYS {
        obj.insert(k.into(), stored.get(k).cloned().unwrap_or(Value::Null));
    }
    obj
}

/// AssetResponseDto (asset details screen, PUT /assets/{id}).
pub fn asset_response(a: &Asset, owner: &User, exif: Option<&str>) -> Value {
    let mut v = json!({
        "id": a.id,
        "ownerId": a.owner_id,
        "owner": {
            "id": owner.id,
            "email": owner.email,
            "name": owner.name,
            "avatarColor": "primary",
            "profileImagePath": "",
            "profileChangedAt": iso(owner.profile_changed_at),
        },
        "originalFileName": a.original_file_name,
        "originalMimeType": a.mime,
        "originalPath": a.path,
        "checksum": B64.encode(&a.checksum),
        "type": a.kind,
        "visibility": a.visibility,
        "isArchived": a.visibility == "archive",
        "isFavorite": a.is_favorite,
        "isTrashed": a.deleted_at.is_some(),
        "isOffline": false,
        "isEdited": false,
        "hasMetadata": exif.is_some(),
        "resized": true,
        "createdAt": iso(a.created_at),
        "updatedAt": iso(a.updated_at),
        "fileCreatedAt": iso(a.file_created_at),
        "fileModifiedAt": iso(a.file_modified_at),
        "localDateTime": local_iso(&a.local_date_time),
        "duration": a.duration_ms,
        "width": a.width,
        "height": a.height,
        "thumbhash": a.thumbhash.as_ref().map(|t| B64.encode(t)),
        "libraryId": null,
        "livePhotoVideoId": null,
        "duplicateId": null,
        "stack": null,
        "people": [],
        "tags": [],
    });
    if let Some(data) = exif {
        v["exifInfo"] = Value::Object(full_exif(data));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_format() {
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso(1_714_564_999_123), "2024-05-01T12:03:19.123Z");
        assert_eq!(local_iso("2024-05-01T14:03:22.000"), "2024-05-01T14:03:22.000Z");
    }

    #[test]
    fn exif_has_every_key() {
        let v = sync_exif_v1("a", r#"{"make":"Google","iso":100}"#);
        let o = v.as_object().unwrap();
        assert_eq!(o.len(), 25); // 24 keys + assetId
        assert_eq!(o["make"], "Google");
        assert_eq!(o["latitude"], Value::Null);
    }
}
