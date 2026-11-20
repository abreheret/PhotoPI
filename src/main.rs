//! PhotoPI: a small photo backup server for a Raspberry Pi that speaks the part of the Immich API the
//! Immich mobile app (v3.4.2) needs: login, backup, timeline (sync stream), viewer, favorites, trash.
//! See docs/API-SUBSET.md.

mod assets;
mod auth;
mod db;
mod dto;
mod error;
mod info;
mod media;
mod state;
mod sync;
mod thumbs;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use state::{AppState, Config};
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::Arc;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
}

fn config_from_env() -> Config {
    let home = env_or("HOME", ".");
    Config {
        photo_root: PathBuf::from(env_or("PHOTO_ROOT", "/mnt/music/photo")),
        db_path: PathBuf::from(env_or("PHOTO_DB", &format!("{home}/.local/share/photopi/photopi.db"))),
        cache_dir: PathBuf::from(env_or("PHOTO_CACHE", &format!("{home}/.cache/photopi"))),
        trash_days: env_or("PHOTO_TRASH_DAYS", "30").parse().unwrap_or(30),
    }
}

async fn not_supported() -> error::ApiError {
    error::ApiError::not_supported()
}

/// Access log: method, path (no query string: it can hold keys), status, duration. Successful thumbnail
/// requests are frequent, so they only show at debug level.
async fn access_log(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let start = std::time::Instant::now();
    let res = next.run(req).await;
    let (status, ms) = (res.status().as_u16(), start.elapsed().as_millis());
    if res.status().is_success() && path.ends_with("/thumbnail") {
        tracing::debug!("{method} {path} {status} {ms}ms");
    } else {
        tracing::info!("{method} {path} {status} {ms}ms");
    }
    res
}

pub fn router(st: AppState) -> Router {
    let api = Router::new()
        .route("/server/ping", get(info::ping))
        .route("/server/version", get(info::version))
        .route("/server/features", get(info::features))
        .route("/server/config", get(info::config))
        .route("/server/storage", get(info::storage))
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/validateToken", post(auth::validate_token))
        .route("/auth/status", get(auth::status))
        .route("/users/me", get(info::me))
        .route("/users/me/preferences", get(info::preferences))
        .route(
            "/assets",
            // Uploads can be several GB (videos): no body limit on this one only.
            post(assets::upload).layer(DefaultBodyLimit::disable()).put(assets::update_bulk).delete(assets::delete),
        )
        .route("/assets/{id}", get(assets::info).put(assets::update_one))
        .route("/assets/{id}/thumbnail", get(assets::thumbnail))
        .route("/assets/{id}/original", get(assets::original))
        .route("/assets/{id}/video/playback", get(assets::video_playback))
        .route("/trash/restore/assets", post(assets::trash_restore_assets))
        .route("/trash/restore", post(assets::trash_restore_all))
        .route("/trash/empty", post(assets::trash_empty))
        .route("/sync/stream", post(sync::stream))
        .route("/sync/ack", get(sync::ack_get).post(sync::ack_set).delete(sync::ack_delete))
        .fallback(not_supported);
    Router::new()
        .route("/health", get(|| async { "ok" }))
        // The app learns the API prefix here; without it, it calls /server/version instead of /api/server/version.
        .route("/.well-known/immich", get(|| async { axum::Json(serde_json::json!({ "api": { "endpoint": "/api" } })) }))
        .nest("/api", api)
        .layer(axum::middleware::from_fn(access_log))
        .with_state(st)
}

/// Builds the state and starts the background tasks (thumbnails, trash purge).
pub fn start(cfg: Config) -> anyhow::Result<AppState> {
    db::init(&cfg.db_path)?;
    let (thumbs, rx) = thumbs::Thumbnailer::new(&cfg.cache_dir, &cfg.photo_root, &cfg.db_path);
    let cfg = Arc::new(cfg);
    tokio::spawn(thumbs::worker(thumbs.clone(), rx));
    tokio::spawn(assets::purge_loop(cfg.clone(), thumbs.clone()));
    Ok(AppState { cfg, thumbs })
}

async fn serve(cfg: Config) -> anyhow::Result<()> {
    // Never create PHOTO_ROOT: if the disk is not mounted, refuse to start instead of filling the SD card.
    if !cfg.photo_root.is_dir() {
        anyhow::bail!("PHOTO_ROOT {} does not exist: is the disk mounted?", cfg.photo_root.display());
    }
    let tmp = cfg.photo_root.join(".upload-tmp");
    std::fs::create_dir_all(&tmp)?;
    for e in std::fs::read_dir(&tmp)?.flatten() {
        if e.path().extension().is_some_and(|x| x == "part") {
            let _ = std::fs::remove_file(e.path()); // uploads cut by a restart
        }
    }
    if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
        tracing::warn!("ffmpeg not found: no video thumbnails, no HEIC previews");
    }
    let bind = env_or("PHOTO_BIND", "0.0.0.0:2283");
    tracing::info!("photo_root={} db={} cache={} bind={}", cfg.photo_root.display(), cfg.db_path.display(), cfg.cache_dir.display(), bind);
    let st = start(cfg)?;
    let users: i64 = st.db(|c| Ok(c.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?)).await.map_err(|e| anyhow::anyhow!(e.message))?;
    if users == 0 {
        tracing::warn!("no user yet: create one with `photopi user add <email> <name>`");
    }
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, router(st)).with_graceful_shutdown(shutdown()).await?;
    Ok(())
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("shutting down");
}

/// One line from stdin, without echo when stdin is a terminal. Only the line ending is removed:
/// spaces are part of the password.
fn read_line_hidden(prompt: &str) -> anyhow::Result<String> {
    use std::io::Write;
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let fd = libc::STDIN_FILENO;
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    let is_tty = unsafe { libc::isatty(fd) } == 1 && unsafe { libc::tcgetattr(fd, &mut saved) } == 0;
    if is_tty {
        let mut t = saved;
        t.c_lflag &= !libc::ECHO;
        t.c_lflag |= libc::ECHONL;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) };
    }
    let mut line = String::new();
    let res = std::io::stdin().lock().read_line(&mut line);
    if is_tty {
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
    }
    res?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

/// Asks twice on a terminal; reads one line when stdin is a pipe (scripts).
fn read_password() -> anyhow::Result<String> {
    let interactive = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    let p = if interactive {
        let a = read_line_hidden("password: ")?;
        let b = read_line_hidden("password again: ")?;
        anyhow::ensure!(a == b, "the two passwords are different");
        a
    } else {
        read_line_hidden("password (one line on stdin): ")?
    };
    let n = p.chars().count();
    anyhow::ensure!(n >= 8, "password too short: {n} character(s) received, 8 minimum");
    Ok(p)
}

fn user_cmd(cfg: &Config, args: &[String]) -> anyhow::Result<()> {
    db::init(&cfg.db_path)?;
    let mut c = db::open(&cfg.db_path)?;
    match args.first().map(String::as_str) {
        Some("add") if args.len() >= 3 => {
            let email = args[1].trim();
            let name = args[2..].join(" ");
            let hash = auth::hash_password(&read_password()?)?;
            let id = db::create_user(&mut c, email, &name, &hash)?;
            let label: String = c.query_row("SELECT storage_label FROM users WHERE id = ?1", [&id], |r| r.get(0))?;
            println!("user {email} created (id {id}), photos in {}/{label}", cfg.photo_root.display());
        }
        Some("passwd") if args.len() == 2 => {
            let hash = auth::hash_password(&read_password()?)?;
            let n = c.execute("UPDATE users SET password_hash = ?1 WHERE email = ?2", [&hash, &args[1]])?;
            anyhow::ensure!(n == 1, "no user {}", args[1]);
            // Log out every device of this user.
            c.execute("DELETE FROM sessions WHERE user_id = (SELECT id FROM users WHERE email = ?1)", [&args[1]])?;
            println!("password changed, sessions closed");
        }
        Some("list") => {
            let mut s = c.prepare(
                "SELECT u.email, u.name, u.is_admin, u.storage_label, (SELECT COUNT(*) FROM assets a WHERE a.owner_id = u.id) FROM users u ORDER BY u.created_at",
            )?;
            let rows = s.query_map([], |r| {
                Ok(format!(
                    "{}\t{}\t{}\t{}\t{} assets",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    if r.get::<_, bool>(2)? { "admin" } else { "user" },
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?
                ))
            })?;
            for r in rows {
                println!("{}", r?);
            }
        }
        _ => anyhow::bail!("usage: photopi user add <email> <name> | user passwd <email> | user list"),
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args: Vec<String> = std::env::args().collect();
    let cfg = config_from_env();
    match args.get(1).map(String::as_str) {
        None | Some("serve") => serve(cfg).await,
        Some("user") => user_cmd(&cfg, &args[2..]),
        _ => {
            eprintln!("usage: photopi [serve] | photopi user add <email> <name> | user passwd <email> | user list");
            eprintln!("env: PHOTO_ROOT, PHOTO_DB, PHOTO_CACHE, PHOTO_BIND, PHOTO_TRASH_DAYS, RUST_LOG");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests;
