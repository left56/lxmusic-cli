//! 设置、音源和搜索命中。脚本正文放在 `scripts/`，数据库只存头信息。

use crate::host::ScriptMeta;
use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct NewSource {
    pub name: String,
    pub version: String,
    pub author: String,
    pub description: String,
    pub homepage: String,
    pub origin: String,
    pub content_hash: String,
    pub script: String,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct SourceRow {
    pub id: i64,
    pub name: String,
    pub version: String,
    pub author: String,
    pub description: String,
    pub homepage: String,
    pub origin: String,
    pub script_rel: String,
    pub content_hash: String,
    pub enabled: bool,
    pub status: String,
    pub platforms_json: String,
    pub quality_map_json: String,
    pub last_error: String,
}

impl SourceRow {
    pub fn usable(&self) -> bool {
        self.enabled && self.status != "dead"
    }

    pub fn platforms(&self) -> Vec<String> {
        serde_json::from_str(&self.platforms_json).unwrap_or_default()
    }

    pub fn quality_map(&self) -> BTreeMap<String, Vec<String>> {
        serde_json::from_str(&self.quality_map_json).unwrap_or_default()
    }

    pub fn script_meta(&self, script: &str) -> ScriptMeta {
        ScriptMeta {
            name: self.name.clone(),
            version: self.version.clone(),
            author: self.author.clone(),
            description: self.description.clone(),
            homepage: self.homepage.clone(),
            raw_script: script.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hit {
    pub id: String,
    pub platform: String,
    pub external_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: i64,
    pub qualitys: Vec<String>,
    pub music_info: Value,
}

pub struct Store {
    conn: Connection,
    root: PathBuf,
}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        std::fs::create_dir_all(root.join("scripts"))?;
        let conn = Connection::open(root.join("lxmusic-cli.db"))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS settings (
              key TEXT PRIMARY KEY,
              value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS sources (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              name TEXT NOT NULL UNIQUE,
              version TEXT NOT NULL DEFAULT '',
              author TEXT NOT NULL DEFAULT '',
              description TEXT NOT NULL DEFAULT '',
              homepage TEXT NOT NULL DEFAULT '',
              origin TEXT NOT NULL DEFAULT '',
              script_rel TEXT NOT NULL,
              content_hash TEXT NOT NULL UNIQUE,
              enabled INTEGER NOT NULL DEFAULT 1,
              status TEXT NOT NULL DEFAULT 'unknown',
              platforms TEXT NOT NULL DEFAULT '[]',
              quality_map TEXT NOT NULL DEFAULT '{}',
              last_error TEXT NOT NULL DEFAULT '',
              created_at INTEGER NOT NULL,
              updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS hits (
              id TEXT PRIMARY KEY,
              platform TEXT NOT NULL,
              external_id TEXT NOT NULL,
              title TEXT NOT NULL,
              artist TEXT NOT NULL,
              album TEXT NOT NULL,
              duration INTEGER NOT NULL,
              qualitys TEXT NOT NULL,
              music_info TEXT NOT NULL,
              created_at INTEGER NOT NULL
            );
            ",
        )?;
        Ok(Self {
            conn,
            root: root.to_path_buf(),
        })
    }

    pub fn open_default() -> Result<Self> {
        Self::open(&crate::paths::data_dir())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn list_sources(&self) -> Result<Vec<SourceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, version, author, description, homepage, origin, script_rel,
                    content_hash, enabled, status, platforms, quality_map, last_error
             FROM sources ORDER BY name, id",
        )?;
        let rows = stmt.query_map([], map_source)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    pub fn get_source_by_name(&self, name: &str) -> Result<Option<SourceRow>> {
        self.conn
            .query_row(
                "SELECT id, name, version, author, description, homepage, origin, script_rel,
                        content_hash, enabled, status, platforms, quality_map, last_error
                 FROM sources WHERE name = ?1",
                [name],
                map_source,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn get_source_by_hash(&self, hash: &str) -> Result<Option<SourceRow>> {
        self.conn
            .query_row(
                "SELECT id, name, version, author, description, homepage, origin, script_rel,
                        content_hash, enabled, status, platforms, quality_map, last_error
                 FROM sources WHERE content_hash = ?1",
                [hash],
                map_source,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn insert_source(&self, src: &NewSource) -> Result<i64> {
        let rel = script_rel(&src.content_hash);
        self.write_script(&rel, src.script.as_bytes())?;
        let now = now_secs();
        self.conn.execute(
            "INSERT INTO sources (
                name, version, author, description, homepage, origin, script_rel, content_hash,
                enabled, status, platforms, quality_map, last_error, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'unknown', '[]', '{}', '', ?10, ?10)",
            params![
                src.name,
                src.version,
                src.author,
                src.description,
                src.homepage,
                src.origin,
                rel,
                src.content_hash,
                if src.enabled { 1 } else { 0 },
                now,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// 同名不同内容。保留启用开关，探测状态回到 unknown。
    pub fn overwrite_source(&self, id: i64, src: &NewSource) -> Result<()> {
        let old: String = self
            .conn
            .query_row("SELECT script_rel FROM sources WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .map_err(|_| anyhow!("音源不存在"))?;
        let rel = script_rel(&src.content_hash);
        self.write_script(&rel, src.script.as_bytes())?;
        self.conn.execute(
            "UPDATE sources SET
                version = ?2, author = ?3, description = ?4, homepage = ?5, origin = ?6,
                script_rel = ?7, content_hash = ?8, status = 'unknown', platforms = '[]',
                quality_map = '{}', last_error = '', updated_at = ?9
             WHERE id = ?1",
            params![
                id,
                src.version,
                src.author,
                src.description,
                src.homepage,
                src.origin,
                rel,
                src.content_hash,
                now_secs(),
            ],
        )?;
        if old != rel {
            let _ = std::fs::remove_file(self.root.join(old));
        }
        Ok(())
    }

    pub fn set_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE sources SET enabled = ?2, updated_at = ?3 WHERE name = ?1",
            params![name, if enabled { 1 } else { 0 }, now_secs()],
        )?;
        if n == 0 {
            anyhow::bail!("没有名为「{name}」的音源");
        }
        Ok(())
    }

    pub fn remove_source(&self, name: &str) -> Result<()> {
        let rel: String = self
            .conn
            .query_row(
                "SELECT script_rel FROM sources WHERE name = ?1",
                [name],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("没有名为「{name}」的音源"))?;
        self.conn
            .execute("DELETE FROM sources WHERE name = ?1", [name])?;
        let _ = std::fs::remove_file(self.root.join(rel));
        Ok(())
    }

    pub fn save_runtime_meta(
        &self,
        id: i64,
        platforms: &[String],
        quality_map: &BTreeMap<String, Vec<String>>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sources SET platforms = ?2, quality_map = ?3, updated_at = ?4 WHERE id = ?1",
            params![
                id,
                serde_json::to_string(platforms)?,
                serde_json::to_string(quality_map)?,
                now_secs(),
            ],
        )?;
        Ok(())
    }

    pub fn mark_status(&self, id: i64, status: &str, last_error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sources SET status = ?2, last_error = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, status, last_error, now_secs()],
        )?;
        Ok(())
    }

    pub fn read_script(&self, row: &SourceRow) -> Result<String> {
        let path = self.root.join(&row.script_rel);
        let bytes = std::fs::read(&path).with_context(|| format!("读音源失败 {}", path.display()))?;
        String::from_utf8(bytes).map_err(|_| anyhow!("音源不是 UTF-8"))
    }

    pub fn upsert_hit(&self, hit: &Hit) -> Result<()> {
        self.conn.execute(
            "INSERT INTO hits (
                id, platform, external_id, title, artist, album, duration, qualitys, music_info, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
                platform = excluded.platform,
                external_id = excluded.external_id,
                title = excluded.title,
                artist = excluded.artist,
                album = excluded.album,
                duration = excluded.duration,
                qualitys = excluded.qualitys,
                music_info = excluded.music_info,
                created_at = excluded.created_at",
            params![
                hit.id,
                hit.platform,
                hit.external_id,
                hit.title,
                hit.artist,
                hit.album,
                hit.duration,
                serde_json::to_string(&hit.qualitys)?,
                serde_json::to_string(&hit.music_info)?,
                now_secs(),
            ],
        )?;
        self.conn.execute(
            "DELETE FROM hits WHERE id NOT IN (SELECT id FROM hits ORDER BY created_at DESC LIMIT 2000)",
            [],
        )?;
        Ok(())
    }

    pub fn get_hit(&self, id: &str) -> Result<Option<Hit>> {
        self.conn
            .query_row(
                "SELECT id, platform, external_id, title, artist, album, duration, qualitys, music_info
                 FROM hits WHERE id = ?1",
                [id],
                |row| {
                    let qualitys: String = row.get(7)?;
                    let music_info: String = row.get(8)?;
                    Ok(Hit {
                        id: row.get(0)?,
                        platform: row.get(1)?,
                        external_id: row.get(2)?,
                        title: row.get(3)?,
                        artist: row.get(4)?,
                        album: row.get(5)?,
                        duration: row.get(6)?,
                        qualitys: serde_json::from_str(&qualitys).unwrap_or_default(),
                        music_info: serde_json::from_str(&music_info).unwrap_or(Value::Null),
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    fn write_script(&self, rel: &str, bytes: &[u8]) -> Result<()> {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bytes)?;
        Ok(())
    }
}

pub fn content_hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn script_rel(hash: &str) -> String {
    format!("scripts/{hash}.js")
}

fn map_source(row: &rusqlite::Row<'_>) -> rusqlite::Result<SourceRow> {
    let enabled: i64 = row.get(9)?;
    Ok(SourceRow {
        id: row.get(0)?,
        name: row.get(1)?,
        version: row.get(2)?,
        author: row.get(3)?,
        description: row.get(4)?,
        homepage: row.get(5)?,
        origin: row.get(6)?,
        script_rel: row.get(7)?,
        content_hash: row.get(8)?,
        enabled: enabled != 0,
        status: row.get(10)?,
        platforms_json: row.get(11)?,
        quality_map_json: row.get(12)?,
        last_error: row.get(13)?,
    })
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_and_hits_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert!(store.get_setting("download-dir").unwrap().is_none());
        store.set_setting("download-dir", "/tmp/music").unwrap();
        assert_eq!(
            store.get_setting("download-dir").unwrap().as_deref(),
            Some("/tmp/music")
        );
        let hit = Hit {
            id: "abcd1234".into(),
            platform: "wy".into(),
            external_id: "1".into(),
            title: "晴天".into(),
            artist: "周杰伦".into(),
            album: "叶惠美".into(),
            duration: 269,
            qualitys: vec!["128k".into()],
            music_info: serde_json::json!({"songmid": "1", "source": "wy"}),
        };
        store.upsert_hit(&hit).unwrap();
        let got = store.get_hit("abcd1234").unwrap().unwrap();
        assert_eq!(got.title, "晴天");
        assert_eq!(got.music_info["songmid"], "1");
    }
}
