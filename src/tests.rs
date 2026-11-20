//! End-to-end test of the flows the Immich app uses, through the real router (no network).

use crate::state::Config;
use crate::{auth, db, router, start};
use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::Router;
use base64::Engine;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha1::Digest;
use std::io::Write;
use tower::ServiceExt;

struct Ctx {
    app: Router,
    token: String,
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
}

async fn call(app: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
    let res = app.clone().oneshot(req).await.unwrap();
    let (parts, body) = res.into_parts();
    (parts.status, parts.headers, body.collect().await.unwrap().to_bytes())
}

impl Ctx {
    async fn new() -> Ctx {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("photo");
        std::fs::create_dir_all(&root).unwrap();
        let cfg = Config { photo_root: root.clone(), db_path: dir.path().join("db/p.db"), cache_dir: dir.path().join("cache"), trash_days: 30 };
        db::init(&cfg.db_path).unwrap();
        let mut c = db::open(&cfg.db_path).unwrap();
        db::create_user(&mut c, "amaury@example.com", "Amaury", &auth::hash_password("password123").unwrap()).unwrap();
        let app = router(start(cfg).unwrap());

        // wrong password
        let (s, _, _) = call(&app, json_req(Method::POST, "/api/auth/login", None, json!({"email":"amaury@example.com","password":"nope"}))).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);

        let (s, h, b) = call(&app, json_req(Method::POST, "/api/auth/login", None, json!({"email":"Amaury@example.com","password":"password123"}))).await;
        assert_eq!(s, StatusCode::CREATED);
        let v: Value = serde_json::from_slice(&b).unwrap();
        let token = v["accessToken"].as_str().unwrap().to_string();
        assert!(h.get_all(header::SET_COOKIE).iter().any(|c| c.to_str().unwrap().starts_with(&format!("immich_access_token={token}"))));
        assert_eq!(v["userEmail"], "amaury@example.com");
        Ctx { app, token, _dir: dir, root }
    }

    async fn get(&self, uri: &str) -> (StatusCode, HeaderMap, Bytes) {
        let req = Request::get(uri).header(header::COOKIE, format!("immich_access_token={}", self.token)).body(Body::empty()).unwrap();
        call(&self.app, req).await
    }

    async fn json(&self, method: Method, uri: &str, body: Value) -> (StatusCode, Value) {
        let (s, _, b) = call(&self.app, json_req(method, uri, Some(&self.token), body)).await;
        (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    async fn upload(&self, name: &str, data: &[u8]) -> (StatusCode, Value) {
        let boundary = "PhotoPiTestBoundary";
        let mut body = Vec::new();
        for (k, v) in [
            ("deviceAssetId", "1000"),
            ("deviceId", "pixel"),
            ("fileCreatedAt", "2023-07-14T10:00:00.000Z"),
            ("fileModifiedAt", "2023-07-14T10:00:05.000Z"),
            ("isFavorite", "false"),
            ("duration", "0"),
            ("filename", name),
        ] {
            write!(body, "--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").unwrap();
        }
        write!(body, "--{boundary}\r\nContent-Disposition: form-data; name=\"assetData\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n").unwrap();
        body.extend_from_slice(data);
        write!(body, "\r\n--{boundary}--\r\n").unwrap();
        let req = Request::post("/api/assets")
            .header(header::AUTHORIZATION, format!("Bearer {}", self.token))
            .header(header::CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}"))
            .body(Body::from(body))
            .unwrap();
        let (s, _, b) = call(&self.app, req).await;
        (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    /// Sync stream lines as (type, data, ack).
    async fn sync(&self, types: &[&str]) -> Vec<(String, Value, String)> {
        let req = json_req(Method::POST, "/api/sync/stream", Some(&self.token), json!({ "types": types }));
        let (s, h, b) = call(&self.app, req).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(h[header::CONTENT_TYPE], "application/jsonlines+json");
        String::from_utf8(b.to_vec())
            .unwrap()
            .lines()
            .map(|l| {
                let v: Value = serde_json::from_str(l).unwrap();
                (v["type"].as_str().unwrap().to_string(), v["data"].clone(), v["ack"].as_str().unwrap().to_string())
            })
            .collect()
    }

    async fn ack(&self, lines: &[(String, Value, String)]) {
        let acks: Vec<&str> = lines.iter().map(|l| l.2.as_str()).collect();
        let (s, _) = self.json(Method::POST, "/api/sync/ack", json!({ "acks": acks })).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
    }
}

fn json_req(method: Method, uri: &str, token: Option<&str>, body: Value) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri).header(header::CONTENT_TYPE, "application/json");
    if let Some(t) = token {
        b = b.header("x-immich-user-token", t);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn jpeg(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x * 3) as u8, (y * 5) as u8, 90]));
    let mut out = Vec::new();
    image::DynamicImage::ImageRgb8(img).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Jpeg).unwrap();
    out
}

const TYPES: [&str; 6] = ["AuthUsersV2", "UsersV1", "AssetsV2", "AssetExifsV1", "AlbumsV2", "PeopleV1"];

#[tokio::test]
async fn app_flow() {
    let ctx = Ctx::new().await;

    // startup calls: the app first asks where the API is
    let (s, _, b) = call(&ctx.app, Request::get("/.well-known/immich").body(Body::empty()).unwrap()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(serde_json::from_slice::<Value>(&b).unwrap()["api"]["endpoint"], "/api");
    let (s, _, b) = call(&ctx.app, Request::get("/api/server/ping").body(Body::empty()).unwrap()).await;
    assert_eq!((s, &b[..]), (StatusCode::OK, &br#"{"res":"pong"}"#[..]));
    let (s, _, _) = call(&ctx.app, Request::get("/api/users/me").body(Body::empty()).unwrap()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, b) = ctx.get("/api/users/me").await;
    assert_eq!(s, StatusCode::OK);
    let me: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(me["storageLabel"], "amaury");
    assert_eq!(ctx.get("/api/users/me/preferences").await.0, StatusCode::OK);
    assert_eq!(ctx.get("/api/server/storage").await.0, StatusCode::OK);
    let (s, v) = ctx.json(Method::POST, "/api/auth/validateToken", json!({})).await;
    assert_eq!((s, v["authStatus"].as_bool()), (StatusCode::OK, Some(true)));
    let (s, v) = ctx.json(Method::GET, "/api/albums", json!({})).await;
    assert_eq!(s, StatusCode::NOT_IMPLEMENTED, "{v}");

    // upload, then the same bytes again
    let data = jpeg(640, 480);
    let (s, v) = ctx.upload("PXL_20230714_100000000.jpg", &data).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["status"], "created");
    let id = v["id"].as_str().unwrap().to_string();
    let (s, v) = ctx.upload("other-name.jpg", &data).await;
    assert_eq!((s, v["status"].as_str(), v["id"].as_str()), (StatusCode::OK, Some("duplicate"), Some(id.as_str())));
    let (s, _) = ctx.upload("notes.pdf", b"%PDF").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let local = chrono::DateTime::parse_from_rfc3339("2023-07-14T10:00:00Z").unwrap().with_timezone(&chrono::Local);
    let stored = ctx.root.join(format!("amaury/{}/PXL_20230714_100000000.jpg", local.format("%Y/%m")));
    assert_eq!(std::fs::read(&stored).unwrap(), data, "file not at {}", stored.display());
    assert_eq!(std::fs::read_dir(ctx.root.join(".upload-tmp")).unwrap().count(), 0, "temp files left");

    // wait for the background worker (thumbnails + thumbhash), so the sync below is stable
    let mut info = Value::Null;
    for _ in 0..100 {
        let (_, _, b) = ctx.get(&format!("/api/assets/{id}")).await;
        info = serde_json::from_slice(&b).unwrap();
        if !info["thumbhash"].is_null() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(!info["thumbhash"].is_null(), "no thumbhash: {info}");
    assert_eq!((info["width"].as_i64(), info["height"].as_i64()), (Some(640), Some(480)));
    assert_eq!(info["exifInfo"]["fileSizeInByte"].as_u64(), Some(data.len() as u64));

    // first sync: everything, then the end marker
    let lines = ctx.sync(&TYPES).await;
    let kinds: Vec<&str> = lines.iter().map(|l| l.0.as_str()).collect();
    assert_eq!(kinds, ["AuthUserV2", "UserV1", "AssetV2", "AssetExifV1", "SyncCompleteV1"]);
    let asset = &lines[2].1;
    let sha1 = base64::engine::general_purpose::STANDARD.encode(sha1::Sha1::digest(&data));
    assert_eq!(asset["checksum"], sha1);
    assert_eq!(asset["id"], id.as_str());
    assert_eq!(asset["type"], "IMAGE");
    assert_eq!(asset["fileCreatedAt"], "2023-07-14T10:00:00.000Z");
    assert!(asset["deletedAt"].is_null());
    assert_eq!(lines[3].1["assetId"], id.as_str());
    assert!(lines[2].2.starts_with("AssetV2|"));
    ctx.ack(&lines).await;

    // nothing changed: only the end marker
    let lines = ctx.sync(&TYPES).await;
    assert_eq!(lines.iter().map(|l| l.0.as_str()).collect::<Vec<_>>(), ["SyncCompleteV1"]);

    // viewer
    let (s, h, b) = ctx.get(&format!("/api/assets/{id}/thumbnail?size=thumbnail&edited=false")).await;
    assert_eq!((s, h[header::CONTENT_TYPE].to_str().unwrap()), (StatusCode::OK, "image/jpeg"));
    assert_eq!(image::load_from_memory(&b).unwrap().height(), 250);
    let (s, _, b) = ctx.get(&format!("/api/assets/{id}/thumbnail?size=preview")).await;
    assert_eq!((s, image::load_from_memory(&b).unwrap().height()), (StatusCode::OK, 480)); // never enlarged
    let (s, _, b) = ctx.get(&format!("/api/assets/{id}/original")).await;
    assert_eq!((s, b.to_vec()), (StatusCode::OK, data.clone()));
    let req = Request::get(format!("/api/assets/{id}/original"))
        .header("x-immich-user-token", &ctx.token)
        .header(header::RANGE, "bytes=0-9")
        .body(Body::empty())
        .unwrap();
    let (s, _, b) = call(&ctx.app, req).await;
    assert_eq!((s, b.len()), (StatusCode::PARTIAL_CONTENT, 10));
    assert_eq!(ctx.get(&format!("/api/assets/{id}/video/playback")).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(ctx.get("/api/assets/00000000-0000-4000-8000-000000000000").await.0, StatusCode::NOT_FOUND);

    // favorite
    let (s, v) = ctx.json(Method::PUT, &format!("/api/assets/{id}"), json!({ "isFavorite": true })).await;
    assert_eq!((s, v["isFavorite"].as_bool()), (StatusCode::OK, Some(true)));
    let (s, _) = ctx.json(Method::PUT, &format!("/api/assets/{id}"), json!({ "latitude": 1.0 })).await;
    assert_eq!(s, StatusCode::NOT_IMPLEMENTED);
    let lines = ctx.sync(&TYPES).await;
    assert_eq!(lines[0].0, "AssetV2");
    assert_eq!(lines[0].1["isFavorite"], true);
    ctx.ack(&lines).await;

    // trash, restore
    let (s, _) = ctx.json(Method::DELETE, "/api/assets", json!({ "ids": [id] })).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let lines = ctx.sync(&TYPES).await;
    assert_eq!(lines[0].0, "AssetV2");
    assert!(lines[0].1["deletedAt"].is_string());
    ctx.ack(&lines).await;
    assert!(stored.exists(), "trash must keep the file");
    let (s, v) = ctx.json(Method::POST, "/api/trash/restore/assets", json!({ "assetIds": [id] })).await;
    assert_eq!((s, v["count"].as_i64()), (StatusCode::OK, Some(1)));

    // permanent delete
    let (s, _) = ctx.json(Method::DELETE, "/api/assets", json!({ "ids": [id], "force": true })).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let lines = ctx.sync(&TYPES).await;
    let kinds: Vec<&str> = lines.iter().map(|l| l.0.as_str()).collect();
    assert_eq!(kinds, ["AssetDeleteV1", "SyncCompleteV1"]); // the row is gone: no AssetV2
    assert_eq!(lines[0].1["assetId"], id.as_str());
    assert!(!stored.exists());
    ctx.ack(&lines).await;

    // reset ack: everything again from the start (here: the users)
    ctx.ack(&[("SyncResetV1".into(), Value::Null, "SyncResetV1|reset".into())]).await;
    let lines = ctx.sync(&TYPES).await;
    assert_eq!(lines.iter().map(|l| l.0.as_str()).collect::<Vec<_>>(), ["AuthUserV2", "UserV1", "AssetDeleteV1", "SyncCompleteV1"]);

    // logout ends the session
    let (s, _) = ctx.json(Method::POST, "/api/auth/logout", json!({})).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(ctx.get("/api/users/me").await.0, StatusCode::UNAUTHORIZED);
}
