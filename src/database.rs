use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::{sqlite::SqliteConnectOptions, SqlitePool};
use std::str::FromStr;
use tracing::{debug, info};

pub struct Database {
    pool: SqlitePool,
}

#[derive(Debug, Clone, sqlx::FromRow)]
#[allow(dead_code)]
pub struct NotifiedFile {
    pub id: i64,
    pub entry_id: i64,
    pub entry_name: String,
    pub file_name: String,
    pub file_url: String,
    pub file_size: Option<i64>,
    pub notified_at: DateTime<Utc>,
    pub downloaded: i64,
}

#[derive(Debug, Clone)]
pub struct Subscription {
    pub entry_id: i64,
    pub title: Option<String>,
    pub keywords: Vec<String>,
    pub created_at: DateTime<Utc>,
    /// 是否静音（跳过通知但仍记录）
    pub muted: bool,
    /// 订阅级自动下载：None=跟随全局配置
    pub auto_download: Option<bool>,
    /// 通知后是否直接把字幕文件发到聊天
    pub send_file: bool,
}

/// /sub 多结果选择的暂存记录
#[derive(Debug, Clone)]
pub struct PendingSearch {
    pub chat_id: i64,
    pub keywords: Vec<String>,
    pub entry_ids: Vec<i64>,
}

impl Database {
    pub async fn new(database_url: &str) -> Result<Self> {
        let options = SqliteConnectOptions::from_str(database_url)?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);

        let pool = SqlitePool::connect_with(options)
            .await
            .context("Failed to connect to SQLite database")?;

        let db = Self { pool };
        db.migrate().await?;
        Ok(db)
    }

    async fn migrate(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS notified_files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                entry_id INTEGER NOT NULL,
                entry_name TEXT NOT NULL,
                file_name TEXT NOT NULL,
                file_url TEXT NOT NULL UNIQUE,
                file_size INTEGER,
                notified_at TEXT NOT NULL,
                downloaded INTEGER DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_notified_entry ON notified_files(entry_id);
            CREATE INDEX IF NOT EXISTS idx_notified_url ON notified_files(file_url);
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to create notified_files table")?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS state (
                key TEXT PRIMARY KEY,
                value TEXT
            );
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to create state table")?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS subscriptions (
                entry_id INTEGER PRIMARY KEY,
                title TEXT,
                keywords TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL
            );
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to create subscriptions table")?;

        self.ensure_column("subscriptions", "title", "TEXT").await?;
        self.ensure_column("subscriptions", "keywords", "TEXT NOT NULL DEFAULT '[]'")
            .await?;
        self.ensure_column("subscriptions", "muted", "INTEGER NOT NULL DEFAULT 0")
            .await?;
        // NULL=跟随全局下载配置, 0=关, 1=开
        self.ensure_column("subscriptions", "auto_download", "INTEGER")
            .await?;
        self.ensure_column("subscriptions", "send_file", "INTEGER NOT NULL DEFAULT 0")
            .await?;
        // notified: 1=已通知, 0=待通知(占位), -1=死信(重试耗尽)
        // 历史行默认 1，避免升级后重复推送
        self.ensure_column("notified_files", "notified", "INTEGER NOT NULL DEFAULT 1")
            .await?;
        self.ensure_column("notified_files", "attempts", "INTEGER NOT NULL DEFAULT 0")
            .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS pending_searches (
                token TEXT PRIMARY KEY,
                chat_id INTEGER NOT NULL,
                keywords TEXT NOT NULL DEFAULT '[]',
                entry_ids TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL
            );
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to create pending_searches table")?;

        // 频道全量推送：pushed 1=已推送(含静默基线), 0=待推送(占位), -1=死信
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS channel_files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                entry_id INTEGER NOT NULL,
                entry_name TEXT NOT NULL,
                file_name TEXT NOT NULL,
                file_url TEXT NOT NULL UNIQUE,
                file_size INTEGER,
                pushed INTEGER NOT NULL DEFAULT 0,
                attempts INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                pushed_at TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_channel_files_entry ON channel_files(entry_id);
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to create channel_files table")?;

        // AniList 罗马音标题缓存；romaji 为 NULL 表示负缓存（查无此条目）
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS anilist_titles (
                anilist_id INTEGER PRIMARY KEY,
                romaji TEXT,
                fetched_at TEXT NOT NULL
            );
            "#,
        )
        .execute(&self.pool)
        .await
        .context("Failed to create anilist_titles table")?;

        info!("Database migration completed");
        Ok(())
    }

    async fn ensure_column(&self, table: &str, column: &str, definition: &str) -> Result<()> {
        let pragma = format!("PRAGMA table_info({})", table);
        let rows: Vec<(i64, String, String, i64, Option<String>, i64)> =
            sqlx::query_as(&pragma).fetch_all(&self.pool).await?;
        let exists = rows.iter().any(|(_, name, _, _, _, _)| name == column);

        if !exists {
            let sql = format!("ALTER TABLE {} ADD COLUMN {} {}", table, column, definition);
            sqlx::query(&sql).execute(&self.pool).await?;
        }

        Ok(())
    }

    pub async fn get_last_check_time(&self) -> Result<Option<i64>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT value FROM state WHERE key = 'last_check_time'")
                .fetch_optional(&self.pool)
                .await?;

        match row {
            Some((value,)) => {
                let ts = value.parse::<i64>().ok();
                debug!("Last check time: {:?}", ts);
                Ok(ts)
            }
            None => Ok(None),
        }
    }

    pub async fn set_last_check_time(&self, timestamp: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO state (key, value) VALUES ('last_check_time', ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(timestamp.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn is_file_notified(&self, file_url: &str) -> Result<bool> {
        let count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM notified_files WHERE file_url = ? AND notified = 1",
        )
        .bind(file_url)
        .fetch_one(&self.pool)
        .await?;
        Ok(count.0 > 0)
    }

    /// 插入/更新占位行（notified=0），返回行 id。
    /// 已死信(-1)的行保持死信状态，不再重试。
    pub async fn upsert_pending_file(
        &self,
        entry_id: i64,
        entry_name: &str,
        file_name: &str,
        file_url: &str,
        file_size: Option<i64>,
        downloaded: bool,
    ) -> Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO notified_files
             (entry_id, entry_name, file_name, file_url, file_size, notified_at, downloaded, notified, attempts)
             VALUES (?, ?, ?, ?, ?, ?, ?, 0, 0)
             ON CONFLICT(file_url) DO UPDATE SET
                entry_id = excluded.entry_id,
                entry_name = excluded.entry_name,
                file_name = excluded.file_name,
                file_size = COALESCE(excluded.file_size, notified_files.file_size),
                downloaded = MAX(notified_files.downloaded, excluded.downloaded),
                notified = CASE WHEN notified_files.notified = -1 THEN -1 ELSE 0 END
             RETURNING id",
        )
        .bind(entry_id)
        .bind(entry_name)
        .bind(file_name)
        .bind(file_url)
        .bind(file_size)
        .bind(Utc::now().to_rfc3339())
        .bind(if downloaded { 1 } else { 0 })
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    pub async fn mark_notified(&self, id: i64) -> Result<()> {
        sqlx::query("UPDATE notified_files SET notified = 1, notified_at = ? WHERE id = ?")
            .bind(Utc::now().to_rfc3339())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 记录一次通知失败，返回当前失败次数；达到上限后转为死信(notified=-1)。
    pub async fn mark_notify_failed(&self, id: i64, max_attempts: i64) -> Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "UPDATE notified_files
             SET attempts = attempts + 1,
                 notified = CASE WHEN attempts + 1 >= ? THEN -1 ELSE notified END
             WHERE id = ?
             RETURNING attempts",
        )
        .bind(max_attempts)
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    pub async fn is_file_downloaded(&self, file_url: &str) -> Result<bool> {
        let count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM notified_files WHERE file_url = ? AND downloaded = 1",
        )
        .bind(file_url)
        .fetch_one(&self.pool)
        .await?;
        Ok(count.0 > 0)
    }

    pub async fn record_file(
        &self,
        entry_id: i64,
        entry_name: &str,
        file_name: &str,
        file_url: &str,
        file_size: Option<i64>,
        downloaded: bool,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO notified_files
             (entry_id, entry_name, file_name, file_url, file_size, notified_at, downloaded, notified)
             VALUES (?, ?, ?, ?, ?, ?, ?, 1)
             ON CONFLICT(file_url) DO UPDATE SET
                entry_id = excluded.entry_id,
                entry_name = excluded.entry_name,
                file_name = excluded.file_name,
                file_size = COALESCE(excluded.file_size, notified_files.file_size),
                downloaded = MAX(notified_files.downloaded, excluded.downloaded)",
        )
        .bind(entry_id)
        .bind(entry_name)
        .bind(file_name)
        .bind(file_url)
        .bind(file_size)
        .bind(Utc::now().to_rfc3339())
        .bind(if downloaded { 1 } else { 0 })
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn add_subscription_details(
        &self,
        entry_id: i64,
        title: Option<&str>,
        keywords: &[String],
    ) -> Result<bool> {
        let keyword_json = serde_json::to_string(keywords)?;
        let result = sqlx::query(
            "INSERT INTO subscriptions (entry_id, title, keywords, created_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(entry_id) DO UPDATE SET
                title = COALESCE(excluded.title, subscriptions.title),
                keywords = excluded.keywords",
        )
        .bind(entry_id)
        .bind(title)
        .bind(keyword_json)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn update_subscription_title(&self, entry_id: i64, title: &str) -> Result<bool> {
        let result = sqlx::query("UPDATE subscriptions SET title = ? WHERE entry_id = ?")
            .bind(title)
            .bind(entry_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn remove_subscription(&self, entry_id: i64) -> Result<bool> {
        let result = sqlx::query("DELETE FROM subscriptions WHERE entry_id = ?")
            .bind(entry_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn remove_subscription_by_name(&self, name: &str) -> Result<bool> {
        let normalized = name.trim().to_lowercase();
        let result = sqlx::query(
            "DELETE FROM subscriptions
             WHERE lower(title) = ?",
        )
        .bind(&normalized)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn list_subscriptions(&self) -> Result<Vec<Subscription>> {
        type Row = (
            i64,
            Option<String>,
            Option<String>,
            String,
            i64,
            Option<i64>,
            i64,
        );
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT entry_id, title, keywords, created_at, muted, auto_download, send_file
                 FROM subscriptions ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await?;

        let subs = rows
            .into_iter()
            .filter_map(
                |(entry_id, title, keywords, ts, muted, auto_download, send_file)| {
                    DateTime::parse_from_rfc3339(&ts)
                        .ok()
                        .map(|dt| Subscription {
                            entry_id,
                            title,
                            keywords: parse_keywords(keywords.as_deref()),
                            created_at: dt.with_timezone(&Utc),
                            muted: muted != 0,
                            auto_download: auto_download.map(|v| v != 0),
                            send_file: send_file != 0,
                        })
                },
            )
            .collect();
        Ok(subs)
    }

    pub async fn get_subscription(&self, entry_id: i64) -> Result<Option<Subscription>> {
        Ok(self
            .list_subscriptions()
            .await?
            .into_iter()
            .find(|sub| sub.entry_id == entry_id))
    }

    /// 更新订阅级策略（读-改-写整体落库）
    pub async fn save_subscription_policy(
        &self,
        entry_id: i64,
        muted: bool,
        auto_download: Option<bool>,
        send_file: bool,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE subscriptions SET muted = ?, auto_download = ?, send_file = ?
             WHERE entry_id = ?",
        )
        .bind(if muted { 1 } else { 0 })
        .bind(auto_download.map(|v| if v { 1 } else { 0 }))
        .bind(if send_file { 1 } else { 0 })
        .bind(entry_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn create_pending_search(
        &self,
        token: &str,
        chat_id: i64,
        keywords: &[String],
        entry_ids: &[i64],
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO pending_searches (token, chat_id, keywords, entry_ids, created_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(token)
        .bind(chat_id)
        .bind(serde_json::to_string(keywords)?)
        .bind(serde_json::to_string(entry_ids)?)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 读取暂存的搜索记录；惰性清理 24 小时前的过期记录
    pub async fn get_pending_search(&self, token: &str) -> Result<Option<PendingSearch>> {
        let cutoff = (Utc::now() - chrono::Duration::hours(24)).to_rfc3339();
        sqlx::query("DELETE FROM pending_searches WHERE created_at < ?")
            .bind(&cutoff)
            .execute(&self.pool)
            .await?;

        let row: Option<(i64, String, String)> = sqlx::query_as(
            "SELECT chat_id, keywords, entry_ids FROM pending_searches WHERE token = ?",
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.and_then(|(chat_id, keywords, entry_ids)| {
            let keywords = serde_json::from_str(&keywords).ok()?;
            let entry_ids = serde_json::from_str(&entry_ids).ok()?;
            Some(PendingSearch {
                chat_id,
                keywords,
                entry_ids,
            })
        }))
    }

    pub async fn delete_pending_search(&self, token: &str) -> Result<()> {
        sqlx::query("DELETE FROM pending_searches WHERE token = ?")
            .bind(token)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_notified_file_by_id(&self, id: i64) -> Result<Option<NotifiedFile>> {
        type Row = (i64, i64, String, String, String, Option<i64>, String, i64);
        let row: Option<Row> = sqlx::query_as(
            "SELECT id, entry_id, entry_name, file_name, file_url, file_size, notified_at, downloaded
                 FROM notified_files WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.and_then(
            |(id, entry_id, entry_name, file_name, file_url, file_size, ts, downloaded)| {
                DateTime::parse_from_rfc3339(&ts)
                    .ok()
                    .map(|dt| NotifiedFile {
                        id,
                        entry_id,
                        entry_name,
                        file_name,
                        file_url,
                        file_size,
                        notified_at: dt.with_timezone(&Utc),
                        downloaded,
                    })
            },
        ))
    }

    pub async fn get_notified_files_count(&self) -> Result<i64> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM notified_files")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0)
    }

    pub async fn get_downloaded_count(&self) -> Result<i64> {
        let row: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM notified_files WHERE downloaded = 1")
                .fetch_one(&self.pool)
                .await?;
        Ok(row.0)
    }

    // ---------- 频道全量推送 ----------

    /// 读取频道推送基线时间（首次启用时由 scheduler 写入）
    pub async fn get_channel_enabled_at(&self) -> Result<Option<DateTime<Utc>>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT value FROM state WHERE key = 'channel_enabled_at'")
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(v,)| {
            DateTime::parse_from_rfc3339(&v)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        }))
    }

    /// 写入频道推送基线时间；已存在则保持不变（只写一次）
    pub async fn ensure_channel_enabled_at(&self, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
        sqlx::query(
            "INSERT INTO state (key, value) VALUES ('channel_enabled_at', ?)
             ON CONFLICT(key) DO NOTHING",
        )
        .bind(now.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(self.get_channel_enabled_at().await?.unwrap_or(now))
    }

    pub async fn is_channel_pushed(&self, file_url: &str) -> Result<bool> {
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM channel_files WHERE file_url = ? AND pushed != 0")
                .bind(file_url)
                .fetch_one(&self.pool)
                .await?;
        Ok(count.0 > 0)
    }

    /// 静默标记文件已推送（基线前的 backlog，不发频道）
    pub async fn mark_channel_silent(
        &self,
        entry_id: i64,
        entry_name: &str,
        file_name: &str,
        file_url: &str,
        file_size: Option<i64>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO channel_files
             (entry_id, entry_name, file_name, file_url, file_size, pushed, attempts, created_at, pushed_at)
             VALUES (?, ?, ?, ?, ?, 1, 0, ?, ?)
             ON CONFLICT(file_url) DO NOTHING",
        )
        .bind(entry_id)
        .bind(entry_name)
        .bind(file_name)
        .bind(file_url)
        .bind(file_size)
        .bind(Utc::now().to_rfc3339())
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 插入/更新频道推送占位行（pushed=0），返回行 id。死信(-1)行保持死信。
    pub async fn upsert_channel_pending(
        &self,
        entry_id: i64,
        entry_name: &str,
        file_name: &str,
        file_url: &str,
        file_size: Option<i64>,
    ) -> Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO channel_files
             (entry_id, entry_name, file_name, file_url, file_size, pushed, attempts, created_at)
             VALUES (?, ?, ?, ?, ?, 0, 0, ?)
             ON CONFLICT(file_url) DO UPDATE SET
                entry_id = excluded.entry_id,
                entry_name = excluded.entry_name,
                file_name = excluded.file_name,
                file_size = COALESCE(excluded.file_size, channel_files.file_size),
                pushed = CASE WHEN channel_files.pushed = -1 THEN -1 ELSE 0 END
             RETURNING id",
        )
        .bind(entry_id)
        .bind(entry_name)
        .bind(file_name)
        .bind(file_url)
        .bind(file_size)
        .bind(Utc::now().to_rfc3339())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    pub async fn mark_channel_pushed(&self, id: i64) -> Result<()> {
        sqlx::query("UPDATE channel_files SET pushed = 1, pushed_at = ? WHERE id = ?")
            .bind(Utc::now().to_rfc3339())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// 记录一次频道推送失败，返回当前失败次数；达到上限转死信(pushed=-1)。
    pub async fn mark_channel_failed(&self, id: i64, max_attempts: i64) -> Result<i64> {
        let row: (i64,) = sqlx::query_as(
            "UPDATE channel_files
             SET attempts = attempts + 1,
                 pushed = CASE WHEN attempts + 1 >= ? THEN -1 ELSE pushed END
             WHERE id = ?
             RETURNING attempts",
        )
        .bind(max_attempts)
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// 列出待推送（pushed=0）的频道文件，用于每轮独立重试
    pub async fn list_channel_pending(&self) -> Result<Vec<ChannelFile>> {
        let rows: Vec<(i64, i64, String, String, String, Option<i64>)> = sqlx::query_as(
            "SELECT id, entry_id, entry_name, file_name, file_url, file_size
             FROM channel_files WHERE pushed = 0 ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, entry_id, entry_name, file_name, file_url, file_size)| ChannelFile {
                    id,
                    entry_id,
                    entry_name,
                    file_name,
                    file_url,
                    file_size,
                },
            )
            .collect())
    }

    // ---------- AniList 标题缓存 ----------

    /// 读取缓存的罗马音标题。Ok(None)=未缓存；Ok(Some(None))=负缓存（查无条目）
    pub async fn get_cached_romaji(&self, anilist_id: i32) -> Result<Option<Option<String>>> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT romaji FROM anilist_titles WHERE anilist_id = ?")
                .bind(anilist_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(romaji,)| romaji))
    }

    pub async fn cache_romaji(&self, anilist_id: i32, romaji: Option<&str>) -> Result<()> {
        sqlx::query(
            "INSERT INTO anilist_titles (anilist_id, romaji, fetched_at) VALUES (?, ?, ?)
             ON CONFLICT(anilist_id) DO UPDATE SET romaji = excluded.romaji, fetched_at = excluded.fetched_at",
        )
        .bind(anilist_id)
        .bind(romaji)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// 待推送的频道文件记录
#[derive(Debug, Clone)]
pub struct ChannelFile {
    pub id: i64,
    pub entry_id: i64,
    pub entry_name: String,
    pub file_name: String,
    pub file_url: String,
    pub file_size: Option<i64>,
}

fn parse_keywords(value: Option<&str>) -> Vec<String> {
    value
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::Database;

    fn temp_db_url(tag: &str) -> (String, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("jimaku-test-{}-{}", tag, uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}/test.db", dir.display());
        (url, dir)
    }

    #[tokio::test]
    async fn migrates_and_roundtrips() {
        let (url, dir) = temp_db_url("fresh");
        let db = Database::new(&url).await.unwrap();

        // 占位-确认通知流程
        let id = db
            .upsert_pending_file(1, "entry", "file.ass", "http://x/f", Some(10), false)
            .await
            .unwrap();
        assert!(!db.is_file_notified("http://x/f").await.unwrap());
        db.mark_notified(id).await.unwrap();
        assert!(db.is_file_notified("http://x/f").await.unwrap());
        assert_eq!(
            db.get_notified_file_by_id(id)
                .await
                .unwrap()
                .unwrap()
                .entry_id,
            1
        );

        // 失败重试达到上限转死信
        let id2 = db
            .upsert_pending_file(1, "entry", "dead.ass", "http://x/d", None, false)
            .await
            .unwrap();
        for _ in 0..5 {
            db.mark_notify_failed(id2, 5).await.unwrap();
        }
        // 死信行不再被 upsert 复活为待通知
        db.upsert_pending_file(1, "entry", "dead.ass", "http://x/d", None, false)
            .await
            .unwrap();
        assert!(!db.is_file_notified("http://x/d").await.unwrap());

        // 订阅策略
        db.add_subscription_details(1, Some("title"), &[])
            .await
            .unwrap();
        let sub = db.get_subscription(1).await.unwrap().unwrap();
        assert!(!sub.muted && !sub.send_file && sub.auto_download.is_none());
        db.save_subscription_policy(1, true, Some(true), true)
            .await
            .unwrap();
        let sub = db.get_subscription(1).await.unwrap().unwrap();
        assert!(sub.muted && sub.send_file && sub.auto_download == Some(true));

        // 暂存搜索
        db.create_pending_search("tok12345", 99, &["NF".to_string()], &[1, 2])
            .await
            .unwrap();
        let pending = db.get_pending_search("tok12345").await.unwrap().unwrap();
        assert_eq!(pending.chat_id, 99);
        assert_eq!(pending.entry_ids, vec![1, 2]);
        db.delete_pending_search("tok12345").await.unwrap();
        assert!(db.get_pending_search("tok12345").await.unwrap().is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn channel_push_state_machine() {
        let (url, dir) = temp_db_url("channel");
        let db = Database::new(&url).await.unwrap();

        // 基线时间只写一次
        let t1 = db
            .ensure_channel_enabled_at(chrono::Utc::now())
            .await
            .unwrap();
        let t2 = db
            .ensure_channel_enabled_at(chrono::Utc::now() + chrono::Duration::hours(1))
            .await
            .unwrap();
        assert_eq!(t1, t2);

        // 静默标记 backlog
        db.mark_channel_silent(1, "e", "old.srt", "http://x/old", Some(1))
            .await
            .unwrap();
        assert!(db.is_channel_pushed("http://x/old").await.unwrap());

        // 占位 → 确认 roundtrip
        let id = db
            .upsert_channel_pending(1, "e", "new.srt", "http://x/new", None)
            .await
            .unwrap();
        assert!(!db.is_channel_pushed("http://x/new").await.unwrap());
        let pending = db.list_channel_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].file_url, "http://x/new");
        db.mark_channel_pushed(id).await.unwrap();
        assert!(db.is_channel_pushed("http://x/new").await.unwrap());
        assert!(db.list_channel_pending().await.unwrap().is_empty());

        // 5 次失败转死信，死信行 upsert 不复活
        let id2 = db
            .upsert_channel_pending(1, "e", "dead.srt", "http://x/dead", None)
            .await
            .unwrap();
        for _ in 0..5 {
            db.mark_channel_failed(id2, 5).await.unwrap();
        }
        db.upsert_channel_pending(1, "e", "dead.srt", "http://x/dead", None)
            .await
            .unwrap();
        assert!(db.is_channel_pushed("http://x/dead").await.unwrap());
        assert!(db.list_channel_pending().await.unwrap().is_empty());

        // anilist 正/负缓存
        assert!(db.get_cached_romaji(1).await.unwrap().is_none());
        db.cache_romaji(1, Some("Honzuki no Gekokujou"))
            .await
            .unwrap();
        assert_eq!(
            db.get_cached_romaji(1).await.unwrap(),
            Some(Some("Honzuki no Gekokujou".to_string()))
        );
        db.cache_romaji(2, None).await.unwrap();
        assert_eq!(db.get_cached_romaji(2).await.unwrap(), Some(None));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 模拟生产库的旧表结构（无 notified/attempts 列），迁移后历史行必须保持「已通知」
    #[tokio::test]
    async fn migrates_legacy_schema_without_renotify() {
        use std::str::FromStr;
        let (url, dir) = temp_db_url("legacy");

        // 用旧结构先建库并插入一条历史通知记录
        let options = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true);
        let pool = sqlx::SqlitePool::connect_with(options).await.unwrap();
        sqlx::query(
            "CREATE TABLE notified_files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                entry_id INTEGER NOT NULL,
                entry_name TEXT NOT NULL,
                file_name TEXT NOT NULL,
                file_url TEXT NOT NULL UNIQUE,
                file_size INTEGER,
                notified_at TEXT NOT NULL,
                downloaded INTEGER DEFAULT 0
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO notified_files (entry_id, entry_name, file_name, file_url, notified_at)
             VALUES (1, 'e', 'old.ass', 'http://x/old', '2026-05-01T00:00:00+00:00')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let db = Database::new(&url).await.unwrap();
        assert!(db.is_file_notified("http://x/old").await.unwrap());

        std::fs::remove_dir_all(&dir).ok();
    }
}
