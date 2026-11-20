//! Thumbnails (cached on the SD card) and thumbhash.
//!
//! Two JPEGs per asset: `preview` (short side 1440 px, full screen viewer) and `thumbnail` (short side
//! 250 px, timeline grid). They are made by a background worker after each upload, or on the first
//! request if the worker has not got there yet. At most 2 generations run at once: the Pi also runs
//! Plex and JukePI.

use crate::db;
use crate::dto::{asset_from_row, Asset, ASSET_COLS};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, ImageDecoder};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};

const PREVIEW_SHORT_SIDE: u32 = 1440;
const THUMB_SHORT_SIDE: u32 = 250;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    Thumbnail,
    Preview,
}

pub struct Thumbnailer {
    dir: PathBuf,
    photo_root: PathBuf,
    db_path: PathBuf,
    sem: Semaphore,
    queue: mpsc::UnboundedSender<String>,
}

impl Thumbnailer {
    pub fn new(cache_dir: &Path, photo_root: &Path, db_path: &Path) -> (Arc<Self>, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let t = Thumbnailer {
            dir: cache_dir.join("thumbs"),
            photo_root: photo_root.to_path_buf(),
            db_path: db_path.to_path_buf(),
            sem: Semaphore::new(2),
            queue: tx,
        };
        (Arc::new(t), rx)
    }

    pub fn path(&self, id: &str, size: Size) -> PathBuf {
        let prefix = id.get(..2).unwrap_or("00");
        let suffix = match size {
            Size::Thumbnail => "thumbnail",
            Size::Preview => "preview",
        };
        self.dir.join(prefix).join(format!("{id}-{suffix}.jpg"))
    }

    /// Asks the background worker to prepare this asset.
    pub fn enqueue(&self, id: String) {
        let _ = self.queue.send(id);
    }

    pub fn remove(&self, id: &str) {
        for s in [Size::Thumbnail, Size::Preview] {
            let _ = std::fs::remove_file(self.path(id, s));
        }
    }

    fn ready(&self, id: &str) -> bool {
        self.path(id, Size::Thumbnail).exists() && self.path(id, Size::Preview).exists()
    }

    /// Makes both thumbnails if missing. Returns the thumbhash when it generated them.
    pub async fn ensure(self: &Arc<Self>, asset: &Asset) -> anyhow::Result<Option<Vec<u8>>> {
        if self.ready(&asset.id) {
            return Ok(None);
        }
        let _permit = self.sem.acquire().await?;
        if self.ready(&asset.id) {
            return Ok(None); // made by someone else while we waited
        }
        let this = self.clone();
        let src = self.photo_root.join(&asset.path);
        let (id, kind, duration) = (asset.id.clone(), asset.kind.clone(), asset.duration_ms);
        let hash = tokio::task::spawn_blocking(move || {
            let preview = this.path(&id, Size::Preview);
            let thumb = this.path(&id, Size::Thumbnail);
            generate(&src, &kind, duration, &preview, &thumb)
        })
        .await??;
        Ok(Some(hash))
    }
}

/// Decodes a photo with its EXIF orientation applied.
fn decode_image(src: &Path) -> anyhow::Result<DynamicImage> {
    let mut decoder = image::ImageReader::open(src)?.with_guessed_format()?.into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    Ok(img)
}

/// One frame through ffmpeg (videos, and photo formats the image crate cannot read such as HEIC).
/// ffmpeg applies the rotation itself.
fn ffmpeg_frame(src: &Path, seek_s: f64) -> anyhow::Result<DynamicImage> {
    let tmp = std::env::temp_dir().join(format!("photopi-frame-{}.jpg", uuid::Uuid::new_v4()));
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error", "-nostdin"]);
    if seek_s > 0.0 {
        cmd.args(["-ss", &format!("{seek_s:.3}")]);
    }
    cmd.arg("-i").arg(src).args([
        "-frames:v",
        "1",
        "-vf",
        // shrink only (a 720p video must not give a 1440p preview)
        "scale=w='min(2560,iw)':h='min(2560,ih)':force_original_aspect_ratio=decrease",
        "-q:v",
        "3",
        "-y",
    ]);
    cmd.arg(&tmp);
    let out = cmd.output()?;
    let result = if out.status.success() {
        image::open(&tmp).map_err(anyhow::Error::from)
    } else {
        Err(anyhow::anyhow!("ffmpeg: {}", String::from_utf8_lossy(&out.stderr).trim()))
    };
    let _ = std::fs::remove_file(&tmp);
    result
}

fn fit_short_side(img: &DynamicImage, target: u32) -> DynamicImage {
    let (w, h) = img.dimensions();
    let short = w.min(h);
    if short <= target {
        return img.clone();
    }
    let scale = target as f64 / short as f64;
    let nw = ((w as f64 * scale).round() as u32).max(1);
    let nh = ((h as f64 * scale).round() as u32).max(1);
    img.resize_exact(nw, nh, FilterType::Triangle)
}

fn save_jpeg(img: &DynamicImage, out: &Path, quality: u8) -> anyhow::Result<()> {
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Write then rename: a reader never sees half a file, and two concurrent generations are harmless.
    let tmp = out.with_extension(format!("tmp{}", rand::random::<u32>()));
    let res = (|| -> anyhow::Result<()> {
        let mut w = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        img.to_rgb8().write_with_encoder(JpegEncoder::new_with_quality(&mut w, quality))?;
        w.flush()?;
        drop(w);
        std::fs::rename(&tmp, out)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

pub fn thumbhash_of(img: &DynamicImage) -> Vec<u8> {
    let small = img.thumbnail(100, 100).to_rgba8();
    thumbhash::rgba_to_thumb_hash(small.width() as usize, small.height() as usize, small.as_raw())
}

/// Writes preview + thumbnail, returns the thumbhash.
pub fn generate(src: &Path, kind: &str, duration_ms: Option<i64>, preview: &Path, thumb: &Path) -> anyhow::Result<Vec<u8>> {
    let img = if kind == "VIDEO" {
        let seek = if duration_ms.unwrap_or(0) > 2000 { 1.0 } else { 0.0 };
        ffmpeg_frame(src, seek)?
    } else {
        match decode_image(src) {
            Ok(i) => i,
            Err(e) => {
                tracing::debug!("image decode failed for {} ({e}), trying ffmpeg", src.display());
                ffmpeg_frame(src, 0.0)?
            }
        }
    };
    let p = fit_short_side(&img, PREVIEW_SHORT_SIDE);
    drop(img);
    save_jpeg(&p, preview, 80)?;
    let t = fit_short_side(&p, THUMB_SHORT_SIDE);
    save_jpeg(&t, thumb, 80)?;
    Ok(thumbhash_of(&t))
}

fn load_asset(db_path: &Path, id: &str) -> anyhow::Result<Option<Asset>> {
    use rusqlite::OptionalExtension;
    let c = db::open(db_path)?;
    Ok(c.query_row(&format!("SELECT {ASSET_COLS} FROM assets WHERE id = ?1"), [id], asset_from_row).optional()?)
}

/// Stores the thumbhash and bumps update_id so the app receives it through sync.
fn store_thumbhash(db_path: &Path, id: &str, hash: &[u8]) -> anyhow::Result<()> {
    let mut c = db::open(db_path)?;
    let tx = db::write_tx(&mut c)?;
    let uid = db::next_update_id(&tx)?;
    let n = tx.execute(
        "UPDATE assets SET thumbhash = ?1, update_id = ?2 WHERE id = ?3 AND thumbhash IS NULL",
        rusqlite::params![hash, uid, id],
    )?;
    if n == 1 {
        tx.commit()?;
    } // else: nothing to do, the rollback gives the counter value back
    Ok(())
}

/// Background worker: thumbnails + thumbhash for each enqueued asset, one at a time.
pub async fn worker(th: Arc<Thumbnailer>, mut rx: mpsc::UnboundedReceiver<String>) {
    // Assets left without thumbhash (server stopped mid-way, or uploaded before this worker existed).
    let db_path = th.db_path.clone();
    let pending = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<String>> {
        let c = db::open(&db_path)?;
        let mut st = c.prepare("SELECT id FROM assets WHERE thumbhash IS NULL AND deleted_at IS NULL ORDER BY file_created_at DESC")?;
        let ids = st.query_map([], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
        Ok(ids)
    })
    .await;
    if let Ok(Ok(ids)) = pending {
        if !ids.is_empty() {
            tracing::info!("thumbnails: {} assets to prepare", ids.len());
        }
        for id in ids {
            th.enqueue(id);
        }
    }

    while let Some(id) = rx.recv().await {
        let db_path = th.db_path.clone();
        let id2 = id.clone();
        let asset = match tokio::task::spawn_blocking(move || load_asset(&db_path, &id2)).await {
            Ok(Ok(Some(a))) => a,
            Ok(Ok(None)) => continue, // deleted meanwhile
            other => {
                tracing::warn!("thumbnails: cannot load {id}: {other:?}");
                continue;
            }
        };
        if asset.deleted_at.is_some() {
            continue;
        }
        let hash = match th.ensure(&asset).await {
            Ok(Some(h)) => Some(h),
            Ok(None) if asset.thumbhash.is_none() => {
                // Thumbnails exist (made on request) but no hash yet: compute it from the small one.
                let p = th.path(&asset.id, Size::Thumbnail);
                tokio::task::spawn_blocking(move || image::open(p).ok().map(|i| thumbhash_of(&i))).await.ok().flatten()
            }
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("thumbnails: {} ({}): {e:#}", asset.id, asset.path);
                None
            }
        };
        if let (Some(h), None) = (hash, &asset.thumbhash) {
            let db_path = th.db_path.clone();
            let id = asset.id.clone();
            if let Ok(Err(e)) = tokio::task::spawn_blocking(move || store_thumbhash(&db_path, &id, &h)).await {
                tracing::warn!("thumbhash: {e:#}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn makes_thumbnails_from_a_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("p.jpg");
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_fn(3000, 2000, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])));
        save_jpeg(&img, &src, 90).unwrap();
        let (pv, th) = (dir.path().join("a/pv.jpg"), dir.path().join("a/th.jpg"));
        let hash = generate(&src, "IMAGE", None, &pv, &th).unwrap();
        assert!(!hash.is_empty());
        assert_eq!(image::image_dimensions(&pv).unwrap(), (2160, 1440));
        assert_eq!(image::image_dimensions(&th).unwrap(), (375, 250));
    }

    #[test]
    fn video_preview_is_not_enlarged() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("ffmpeg not installed: skipped");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/clip.mp4");
        let (pv, th) = (dir.path().join("pv.jpg"), dir.path().join("th.jpg"));
        generate(&src, "VIDEO", Some(1000), &pv, &th).unwrap();
        assert_eq!(image::image_dimensions(&pv).unwrap(), (64, 36));
        assert_eq!(image::image_dimensions(&th).unwrap(), (64, 36));
    }

    #[test]
    fn small_images_are_not_enlarged() {
        let img = DynamicImage::ImageRgb8(image::RgbImage::new(100, 50));
        assert_eq!(fit_short_side(&img, 250).dimensions(), (100, 50));
    }
}
