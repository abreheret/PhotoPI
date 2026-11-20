//! SQLite storage. The database lives on the SD card (never on the NTFS photo disk).
//!
//! Every row the mobile app syncs carries an `update_id` taken from one global counter. Writes use
//! `BEGIN IMMEDIATE`, so commits happen in counter order and "everything with update_id <= N" is a
//! consistent snapshot for the sync stream (see sync.rs).

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use std::path::Path;
use std::time::Duration;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS counters(name TEXT PRIMARY KEY, value INTEGER NOT NULL);
INSERT OR IGNORE INTO counters(name, value) VALUES('update_id', 0);

CREATE TABLE IF NOT EXISTS users(
  id TEXT PRIMARY KEY,
  email TEXT NOT NULL UNIQUE COLLATE NOCASE,
  name TEXT NOT NULL,
  password_hash TEXT NOT NULL,
  is_admin INTEGER NOT NULL DEFAULT 0,
  -- folder name under PHOTO_ROOT
  storage_label TEXT NOT NULL UNIQUE,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  profile_changed_at INTEGER NOT NULL,
  update_id INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions(
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  -- SHA-256 of the access token (the token itself is never stored)
  token_hash TEXT NOT NULL UNIQUE,
  device_type TEXT NOT NULL DEFAULT '',
  device_os TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

-- Last ack the app sent for each (session, sync entity type).
CREATE TABLE IF NOT EXISTS sync_checkpoints(
  session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  type TEXT NOT NULL,
  ack TEXT NOT NULL,
  PRIMARY KEY(session_id, type)
);

CREATE TABLE IF NOT EXISTS assets(
  id TEXT PRIMARY KEY,
  owner_id TEXT NOT NULL REFERENCES users(id),
  -- relative to PHOTO_ROOT
  path TEXT NOT NULL UNIQUE,
  original_file_name TEXT NOT NULL,
  -- raw SHA-1 of the file (the app compares it with its own hash to skip files already backed up)
  checksum BLOB NOT NULL,
  type TEXT NOT NULL,
  mime TEXT NOT NULL,
  size INTEGER NOT NULL,
  -- as displayed (EXIF orientation applied)
  width INTEGER,
  height INTEGER,
  duration_ms INTEGER,
  -- unix ms (UTC instants)
  file_created_at INTEGER NOT NULL,
  file_modified_at INTEGER NOT NULL,
  -- wall-clock time where the photo was taken, 'YYYY-MM-DDTHH:MM:SS.sss' (the timeline groups by it)
  local_date_time TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  -- set = in the trash
  deleted_at INTEGER,
  is_favorite INTEGER NOT NULL DEFAULT 0,
  visibility TEXT NOT NULL DEFAULT 'timeline',
  thumbhash BLOB,
  device_asset_id TEXT,
  device_id TEXT,
  update_id INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS assets_owner_checksum ON assets(owner_id, checksum);
CREATE INDEX IF NOT EXISTS assets_owner_update ON assets(owner_id, update_id);
CREATE INDEX IF NOT EXISTS assets_deleted ON assets(deleted_at) WHERE deleted_at IS NOT NULL;

-- EXIF as a JSON object with the keys of SyncAssetExifV1 (minus assetId).
CREATE TABLE IF NOT EXISTS exif(
  asset_id TEXT PRIMARY KEY REFERENCES assets(id) ON DELETE CASCADE,
  data TEXT NOT NULL,
  update_id INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS exif_update ON exif(update_id);

-- Permanently deleted assets, so the sync stream can tell the app to drop them.
CREATE TABLE IF NOT EXISTS asset_deletes(
  asset_id TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  update_id INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS asset_deletes_owner_update ON asset_deletes(owner_id, update_id);
";

pub fn open(path: &Path) -> anyhow::Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(10))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL;")?;
    Ok(conn)
}

pub fn init(path: &Path) -> anyhow::Result<()> {
    let conn = open(path)?;
    conn.execute_batch(SCHEMA)?;
    migrate(&conn)?;
    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut s = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = s.query_map([], |r| r.get::<_, String>(1))?.collect::<Result<Vec<_>, _>>()?;
    Ok(names.iter().any(|n| n == column))
}

/// Schema changes after v0.1.0 (databases created before keep working).
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    // 0.2.0: "removed from the server" assets. The file is gone, the row stays (visibility 'hidden')
    // so the app keeps seeing the checksum as backed up and never uploads that photo again.
    if !has_column(conn, "assets", "forgotten")? {
        conn.execute_batch("ALTER TABLE assets ADD COLUMN forgotten INTEGER NOT NULL DEFAULT 0;")?;
    }
    Ok(())
}

/// Write transaction that takes the write lock up front (no lock upgrade deadlocks, commits in update_id order).
pub fn write_tx(conn: &mut Connection) -> rusqlite::Result<Transaction<'_>> {
    conn.transaction_with_behavior(TransactionBehavior::Immediate)
}

/// Next value of the global change counter. Call inside a write transaction.
pub fn next_update_id(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("UPDATE counters SET value = value + 1 WHERE name = 'update_id' RETURNING value", [], |r| r.get(0))
}

/// Last committed change id.
pub fn current_update_id(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT value FROM counters WHERE name = 'update_id'", [], |r| r.get(0))
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[derive(Debug, Clone)]
pub struct User {
    pub id: String,
    pub email: String,
    pub name: String,
    pub is_admin: bool,
    pub storage_label: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub profile_changed_at: i64,
    pub update_id: i64,
}

pub const USER_COLS: &str =
    "id, email, name, is_admin, storage_label, created_at, updated_at, profile_changed_at, update_id";

pub fn user_from_row(r: &rusqlite::Row) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        email: r.get(1)?,
        name: r.get(2)?,
        is_admin: r.get(3)?,
        storage_label: r.get(4)?,
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
        profile_changed_at: r.get(7)?,
        update_id: r.get(8)?,
    })
}

pub fn user_by_email(conn: &Connection, email: &str) -> rusqlite::Result<Option<(User, String)>> {
    conn.query_row(
        &format!("SELECT {USER_COLS}, password_hash FROM users WHERE email = ?1"),
        [email],
        |r| Ok((user_from_row(r)?, r.get(9)?)),
    )
    .optional()
}

/// Folder name for a user: the part of the email before '@', reduced to safe characters.
pub fn storage_label_for(email: &str) -> String {
    let local = email.split('@').next().unwrap_or("user");
    let label: String = local
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c.to_ascii_lowercase() } else { '_' })
        .collect();
    let label = label.trim_matches('.').to_string();
    if label.is_empty() { "user".into() } else { label }
}

/// Creates a user (the first one is admin). Returns its id.
pub fn create_user(conn: &mut Connection, email: &str, name: &str, password_hash: &str) -> anyhow::Result<String> {
    let tx = write_tx(conn)?;
    let count: i64 = tx.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
    let mut label = storage_label_for(email);
    let taken = |l: &str| -> rusqlite::Result<bool> {
        tx.query_row("SELECT EXISTS(SELECT 1 FROM users WHERE storage_label = ?1)", [l], |r| r.get(0))
    };
    let base = label.clone();
    let mut n = 1;
    while taken(&label)? {
        n += 1;
        label = format!("{base}{n}");
    }
    let id = uuid::Uuid::new_v4().to_string();
    let now = now_ms();
    let uid = next_update_id(&tx)?;
    tx.execute(
        "INSERT INTO users(id, email, name, password_hash, is_admin, storage_label, created_at, updated_at, profile_changed_at, update_id)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?7, ?8)",
        rusqlite::params![id, email, name, password_hash, count == 0, label, now, uid],
    )?;
    tx.commit()?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_label() {
        assert_eq!(storage_label_for("Amaury.B@example.com"), "amaury.b");
        assert_eq!(storage_label_for("a b/c@x"), "a_b_c");
        assert_eq!(storage_label_for("@x"), "user");
    }

    #[test]
    fn migration_adds_forgotten_to_an_old_database() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("old.db");
        // a v0.1.0 assets table: no `forgotten` column
        open(&p)
            .unwrap()
            .execute_batch("CREATE TABLE assets(id TEXT PRIMARY KEY, owner_id TEXT, checksum BLOB, deleted_at INTEGER, update_id INTEGER);")
            .unwrap();
        init(&p).unwrap();
        init(&p).unwrap(); // idempotent
        assert!(has_column(&open(&p).unwrap(), "assets", "forgotten").unwrap());
    }

    #[test]
    fn counter_increments() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        init(&p).unwrap();
        let mut c = open(&p).unwrap();
        assert_eq!(current_update_id(&c).unwrap(), 0);
        let tx = write_tx(&mut c).unwrap();
        assert_eq!(next_update_id(&tx).unwrap(), 1);
        assert_eq!(next_update_id(&tx).unwrap(), 2);
        tx.commit().unwrap();
        assert_eq!(current_update_id(&c).unwrap(), 2);
    }
}
