use std::{path::Path, sync::Mutex};

use anyhow::{Result, anyhow};
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paragraph {
    pub id: String,
    pub md5: String,
    pub raw: String,
    pub original: String,
    pub ignored: bool,
    pub attributes: Option<String>,
    pub page: Option<String>,
    pub translation: Option<String>,
    pub engine_name: Option<String>,
    pub target_lang: Option<String>,
}

pub struct TranslationCache(Mutex<Connection>);

impl TranslationCache {
    pub fn open(path: &Path, persistence: bool) -> Result<Self> {
        let conn = if persistence {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            Connection::open(path)?
        } else {
            Connection::open_in_memory()?
        };
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS cache (
               id TEXT PRIMARY KEY, md5 TEXT NOT NULL, raw TEXT, original TEXT,
               ignored INTEGER DEFAULT 0, attributes TEXT, page TEXT,
               translation TEXT, engine_name TEXT, target_lang TEXT
             );
             CREATE INDEX IF NOT EXISTS cache_md5_idx ON cache(md5);
             CREATE TABLE IF NOT EXISTS info (key TEXT UNIQUE, value TEXT);",
        )?;
        migrate_legacy_cache_schema(&conn)?;
        Ok(Self(Mutex::new(conn)))
    }

    pub fn set_info(&self, key: &str, value: &str) -> Result<()> {
        self.0.lock().map_err(|_| anyhow!("缓存锁已损坏"))?.execute(
            "INSERT INTO info VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn get_info(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .0
            .lock()
            .map_err(|_| anyhow!("缓存锁已损坏"))?
            .query_row("SELECT value FROM info WHERE key=?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn save_paragraphs(&self, rows: &[Paragraph]) -> Result<()> {
        let mut conn = self.0.lock().map_err(|_| anyhow!("缓存锁已损坏"))?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO cache
                 (id, md5, raw, original, ignored, attributes, page, translation, engine_name, target_lang)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, NULL)
                 ON CONFLICT(id) DO UPDATE SET
                   md5=excluded.md5,
                   raw=excluded.raw,
                   original=excluded.original,
                   ignored=excluded.ignored,
                   attributes=excluded.attributes,
                   page=excluded.page"
            )?;
            for row in rows {
                stmt.execute(params![
                    row.id,
                    row.md5,
                    row.raw,
                    row.original,
                    row.ignored,
                    row.attributes,
                    row.page
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn untranslated(&self) -> Result<Vec<Paragraph>> {
        self.query("WHERE NOT ignored AND translation IS NULL")
    }

    pub fn all(&self) -> Result<Vec<Paragraph>> {
        self.query("WHERE NOT ignored")
    }

    pub fn all_with_ignored(&self) -> Result<Vec<Paragraph>> {
        self.query("")
    }

    fn query(&self, condition: &str) -> Result<Vec<Paragraph>> {
        let conn = self.0.lock().map_err(|_| anyhow!("缓存锁已损坏"))?;
        let sql = format!(
            "SELECT id, md5, raw, original, ignored, attributes, page,
             translation, engine_name, target_lang FROM cache {condition} ORDER BY rowid"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Paragraph {
                    id: row.get(0)?,
                    md5: row.get(1)?,
                    raw: row.get(2)?,
                    original: row.get(3)?,
                    ignored: row.get(4)?,
                    attributes: row.get(5)?,
                    page: row.get(6)?,
                    translation: row.get(7)?,
                    engine_name: row.get(8)?,
                    target_lang: row.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn update_translations(&self, rows: &[(String, String, String, String)]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut conn = self.0.lock().map_err(|_| anyhow!("缓存锁已损坏"))?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE cache SET translation=?2, engine_name=?3, target_lang=?4 WHERE id=?1",
            )?;
            for row in rows {
                stmt.execute(params![row.0, row.1, row.2, row.3])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn clear_translations(&self, ids: &[String]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.0.lock().map_err(|_| anyhow!("缓存锁已损坏"))?;
        let tx = conn.transaction()?;
        let mut changed = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE cache SET translation=NULL, engine_name=NULL, target_lang=NULL WHERE id=?1",
            )?;
            for id in ids {
                changed += stmt.execute([id])?;
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    pub fn clear_all_translations(&self) -> Result<usize> {
        Ok(self
            .0
            .lock()
            .map_err(|_| anyhow!("缓存锁已损坏"))?
            .execute(
                "UPDATE cache SET translation=NULL, engine_name=NULL, target_lang=NULL WHERE NOT ignored",
                [],
            )?)
    }

    pub fn apply_review(
        &self,
        rows: &[(String, Option<String>, bool, bool)],
        engine: &str,
        target_lang: &str,
    ) -> Result<()> {
        let mut conn = self.0.lock().map_err(|_| anyhow!("缓存锁已损坏"))?;
        let tx = conn.transaction()?;
        for (id, translation, ignored, retranslate) in rows {
            if *retranslate {
                tx.execute(
                    "UPDATE cache SET ignored=0, translation=NULL, engine_name=NULL, target_lang=NULL WHERE id=?1",
                    [id],
                )?;
            } else if *ignored {
                tx.execute(
                    "UPDATE cache SET ignored=1, translation=NULL, engine_name=NULL, target_lang=NULL WHERE id=?1",
                    [id],
                )?;
            } else if let Some(translation) = translation {
                tx.execute(
                    "UPDATE cache SET ignored=0, translation=?2, engine_name=?3, target_lang=?4 WHERE id=?1",
                    params![id, translation, engine, target_lang],
                )?;
            } else {
                tx.execute("UPDATE cache SET ignored=0 WHERE id=?1", [id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn counts(&self) -> Result<(usize, usize)> {
        let conn = self.0.lock().map_err(|_| anyhow!("缓存锁已损坏"))?;
        let translated = conn.query_row(
            "SELECT COUNT(*) FROM cache WHERE NOT ignored AND translation IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        let total = conn.query_row("SELECT COUNT(*) FROM cache WHERE NOT ignored", [], |r| {
            r.get(0)
        })?;
        Ok((translated, total))
    }
}

fn migrate_legacy_cache_schema(conn: &Connection) -> Result<()> {
    let sql = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='cache'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(sql) = sql else {
        return Ok(());
    };
    let normalized = sql.to_ascii_lowercase().replace(['\n', '\r', '\t'], " ");
    if !normalized.contains("md5 text unique") {
        return Ok(());
    }
    conn.execute_batch(
        "BEGIN IMMEDIATE;
         ALTER TABLE cache RENAME TO cache_legacy;
         CREATE TABLE cache (
           id TEXT PRIMARY KEY, md5 TEXT NOT NULL, raw TEXT, original TEXT,
           ignored INTEGER DEFAULT 0, attributes TEXT, page TEXT,
           translation TEXT, engine_name TEXT, target_lang TEXT
         );
         INSERT INTO cache
           (id, md5, raw, original, ignored, attributes, page, translation, engine_name, target_lang)
         SELECT id, md5, raw, original, ignored, attributes, page, translation, engine_name, target_lang
         FROM cache_legacy;
         DROP TABLE cache_legacy;
         CREATE INDEX IF NOT EXISTS cache_md5_idx ON cache(md5);
         COMMIT;"
    )?;
    Ok(())
}

pub fn md5(text: &str) -> String {
    format!("{:x}", md5::compute(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, ignored: bool) -> Paragraph {
        Paragraph {
            id: id.into(),
            md5: format!("m{id}"),
            raw: String::new(),
            original: id.into(),
            ignored,
            attributes: None,
            page: None,
            translation: None,
            engine_name: None,
            target_lang: None,
        }
    }

    #[test]
    fn duplicate_signatures_are_preserved() {
        let cache = TranslationCache::open(Path::new("unused"), false).unwrap();
        let mut first = row("a", false);
        let mut second = row("b", false);
        first.md5 = "same-signature".into();
        second.md5 = "same-signature".into();
        cache.save_paragraphs(&[first, second]).unwrap();
        let rows = cache.all_with_ignored().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].md5, rows[1].md5);
    }

    #[test]
    fn migrates_legacy_unique_md5_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE cache (
                   id TEXT UNIQUE, md5 TEXT UNIQUE, raw TEXT, original TEXT,
                   ignored INTEGER DEFAULT 0, attributes TEXT, page TEXT,
                   translation TEXT, engine_name TEXT, target_lang TEXT
                 );
                 CREATE TABLE info (key TEXT UNIQUE, value TEXT);"
            ).unwrap();
        }
        let cache = TranslationCache::open(&path, true).unwrap();
        let mut first = row("a", false);
        let mut second = row("b", false);
        first.md5 = "same".into();
        second.md5 = "same".into();
        cache.save_paragraphs(&[first, second]).unwrap();
        assert_eq!(cache.all_with_ignored().unwrap().len(), 2);
    }

    #[test]
    fn preserves_order_and_updates_atomically() {
        let cache = TranslationCache::open(Path::new("unused"), false).unwrap();
        cache
            .save_paragraphs(&[row("2", false), row("1", false), row("3", true)])
            .unwrap();
        cache.set_info("title", "Book").unwrap();
        assert_eq!(cache.get_info("title").unwrap().as_deref(), Some("Book"));
        assert_eq!(
            cache
                .all()
                .unwrap()
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            ["2", "1"]
        );
        cache
            .update_translations(&[("2".into(), "二".into(), "e".into(), "zh".into())])
            .unwrap();
        assert_eq!(cache.counts().unwrap(), (1, 2));
        cache
            .apply_review(
                &[("1".into(), Some("一".into()), false, false)],
                "manual",
                "zh",
            )
            .unwrap();
        assert_eq!(cache.counts().unwrap(), (2, 2));
        assert_eq!(cache.clear_all_translations().unwrap(), 2);
        assert_eq!(cache.counts().unwrap(), (0, 2));
        cache
            .apply_review(&[("1".into(), None, true, false)], "manual", "zh")
            .unwrap();
        assert!(cache.all_with_ignored().unwrap()[1].ignored);
    }
}
