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
        // notified: 1=已通知, 0=待通知(占位), -1=死信(重试耗尽)
        // 历史行默认 1，避免升级后重复推送
        self.ensure_column("notified_files", "notified", "INTEGER NOT NULL DEFAULT 1")
            .await?;
        self.ensure_column("notified_files", "attempts", "INTEGER NOT NULL DEFAULT 0")
            .await?;

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
        let rows: Vec<(i64, Option<String>, Option<String>, String)> = sqlx::query_as(
            "SELECT entry_id, title, keywords, created_at
                 FROM subscriptions ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await?;

        let subs = rows
            .into_iter()
            .filter_map(|(entry_id, title, keywords, ts)| {
                DateTime::parse_from_rfc3339(&ts)
                    .ok()
                    .map(|dt| Subscription {
                        entry_id,
                        title,
                        keywords: parse_keywords(keywords.as_deref()),
                        created_at: dt.with_timezone(&Utc),
                    })
            })
            .collect();
        Ok(subs)
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
}

fn parse_keywords(value: Option<&str>) -> Vec<String> {
    value
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
        .unwrap_or_default()
}
