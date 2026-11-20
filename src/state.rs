use crate::error::{ApiError, ApiResult};
use crate::thumbs::Thumbnailer;
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Arc;

pub struct Config {
    /// Where the photos go (on the external disk). Must exist: never created by the server, so an
    /// unmounted disk cannot make it write to the SD card underneath the mount point.
    pub photo_root: PathBuf,
    pub db_path: PathBuf,
    /// Thumbnails cache (on the SD card).
    pub cache_dir: PathBuf,
    pub trash_days: i64,
}

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub thumbs: Arc<Thumbnailer>,
}

impl AppState {
    /// Runs blocking SQLite work on a fresh connection, off the async runtime.
    pub async fn db<T, F>(&self, f: F) -> ApiResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> ApiResult<T> + Send + 'static,
    {
        let path = self.cfg.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = crate::db::open(&path)?;
            f(&mut conn)
        })
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!("blocking task failed: {e}")))?
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.cfg.photo_root.join(".upload-tmp")
    }
}
