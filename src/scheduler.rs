use crate::{
    config::Config,
    database::Database,
    downloader::Downloader,
    jimaku::{Entry, JimakuClient},
    telegram::{NewSubtitle, TelegramNotifier},
};
use anyhow::Result;
use std::sync::Arc;
use tokio::time::{interval, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

pub struct Scheduler {
    config: Config,
    jimaku: JimakuClient,
    db: Arc<Database>,
    notifier: TelegramNotifier,
    downloader: Option<Downloader>,
}

impl Scheduler {
    pub fn new(
        config: Config,
        jimaku: JimakuClient,
        db: Arc<Database>,
        notifier: TelegramNotifier,
    ) -> Self {
        let downloader = if config.download.enabled {
            Some(Downloader::new(&config.download.download_path))
        } else {
            None
        };

        Self {
            config,
            jimaku,
            db,
            notifier,
            downloader,
        }
    }

    pub async fn run(self: Arc<Self>, cancel: CancellationToken) -> Result<()> {
        let mut ticker = interval(Duration::from_secs(self.config.scheduler.interval_seconds));

        // 首次立即执行一次
        if let Err(e) = self.check_once().await {
            error!("Initial check failed: {}", e);
        }

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(e) = self.check_once().await {
                        error!("Scheduled check failed: {}", e);
                    }
                }
                _ = cancel.cancelled() => {
                    info!("Scheduler received shutdown signal, stopping");
                    return Ok(());
                }
            }
        }
    }

    pub async fn check_once(&self) -> Result<()> {
        info!("Starting Jimaku check...");

        let last_check = self.db.get_last_check_time().await?;
        let now = chrono::Utc::now().timestamp();

        // 首次运行：检查最近1小时的条目
        let after = last_check.unwrap_or(now - 3600);

        debug!("Checking entries after timestamp: {}", after);

        let entries = self.search_entries_with_retry(after).await?;
        info!("Found {} entries since last check", entries.len());

        // 合并配置文件订阅和数据库动态订阅
        let dynamic_subs = self.db.list_subscriptions().await?;
        let config_anilist_ids = &self.config.subscription.anilist_ids;
        let config_keywords = &self.config.subscription.name_keywords;
        let is_global =
            config_anilist_ids.is_empty() && config_keywords.is_empty() && dynamic_subs.is_empty();

        for entry in entries {
            let config_match = if config_anilist_ids.is_empty() && config_keywords.is_empty() {
                false
            } else {
                JimakuClient::matches_subscription(&entry, config_anilist_ids, config_keywords)
            };
            let matching_dynamic_subs = dynamic_subs
                .iter()
                .filter(|sub| entry.id == sub.entry_id)
                .collect::<Vec<_>>();

            if !is_global && !config_match && matching_dynamic_subs.is_empty() {
                debug!("Entry {} does not match subscription, skipping", entry.id);
                continue;
            }

            let keyword_filter = if is_global
                || config_match
                || matching_dynamic_subs
                    .iter()
                    .any(|sub| sub.keywords.is_empty())
            {
                None
            } else {
                let keywords = matching_dynamic_subs
                    .iter()
                    .flat_map(|sub| sub.keywords.iter().cloned())
                    .collect::<Vec<_>>();
                Some(keywords)
            };

            if let Err(e) = self.process_entry(&entry, keyword_filter.as_deref()).await {
                error!("Failed to process entry {}: {}", entry.id, e);
            }
        }

        self.db.set_last_check_time(now).await?;
        self.write_heartbeat().await;
        info!("Jimaku check completed");
        Ok(())
    }

    /// Jimaku 搜索失败时有限重试（指数退避 1s/2s/4s）
    async fn search_entries_with_retry(&self, after: i64) -> Result<Vec<Entry>> {
        const MAX_ATTEMPTS: u32 = 3;
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.jimaku.search_entries(Some(after), None).await {
                Ok(entries) => return Ok(entries),
                Err(e) if attempt < MAX_ATTEMPTS => {
                    let backoff = Duration::from_secs(1 << (attempt - 1));
                    warn!(
                        "Jimaku search failed (attempt {}/{}): {}, retrying in {:?}",
                        attempt, MAX_ATTEMPTS, e, backoff
                    );
                    tokio::time::sleep(backoff).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// 每轮成功 check 后刷新心跳文件，供 Docker healthcheck 使用
    async fn write_heartbeat(&self) {
        if let Err(e) = tokio::fs::write("/tmp/jimaku_heartbeat", b"ok").await {
            warn!("Failed to write heartbeat file: {}", e);
        }
    }

    async fn process_entry(&self, entry: &Entry, keyword_filter: Option<&[String]>) -> Result<()> {
        let files = self.jimaku.get_entry_files(entry.id).await?;
        if files.is_empty() {
            return Ok(());
        }

        for file in files {
            if let Some(keywords) = keyword_filter {
                if !file_matches_keywords(&file.name, keywords) {
                    debug!(
                        "File {} does not match subtitle keywords, skipping",
                        file.name
                    );
                    continue;
                }
            }

            if self.db.is_file_notified(&file.url).await? {
                debug!("File already notified: {}", file.name);
                continue;
            }

            let mut downloaded = false;

            if let Some(downloader) = &self.downloader {
                match downloader
                    .download_subtitle(&entry.name, &file.name, &file.url)
                    .await
                {
                    Ok(_) => {
                        downloaded = true;
                    }
                    Err(e) => {
                        warn!("Failed to download {}: {}", file.name, e);
                    }
                }
            }

            // 先占位（notified=0），发送成功才确认，失败留下轮重试
            let file_id = self
                .db
                .upsert_pending_file(
                    entry.id,
                    &entry.name,
                    &file.name,
                    &file.url,
                    Some(file.size),
                    downloaded,
                )
                .await?;

            match self
                .notifier
                .notify_new_subtitle(&NewSubtitle {
                    file_id,
                    entry_name: &entry.name,
                    english_name: entry.english_name.as_deref(),
                    japanese_name: entry.japanese_name.as_deref(),
                    file_name: &file.name,
                    file_size: file.size,
                    file_url: &file.url,
                    file_modified: file.last_modified,
                    entry_id: entry.id,
                    downloaded,
                })
                .await
            {
                Ok(_) => {
                    self.db.mark_notified(file_id).await?;
                }
                Err(e) => {
                    let attempts = self
                        .db
                        .mark_notify_failed(file_id, MAX_NOTIFY_ATTEMPTS)
                        .await?;
                    error!(
                        "Failed to send notification for {} (attempt {}/{}): {}",
                        file.name, attempts, MAX_NOTIFY_ATTEMPTS, e
                    );
                    if attempts >= MAX_NOTIFY_ATTEMPTS {
                        let _ = self
                            .notifier
                            .send_message(format!(
                                "⚠️ 字幕通知重试 {} 次仍失败，已放弃: <code>{}</code>",
                                attempts, file.name
                            ))
                            .await;
                    }
                }
            }

            // 通知间隔，避免触发 Telegram 限流
            tokio::time::sleep(NOTIFY_INTERVAL).await;
        }

        Ok(())
    }
}

const MAX_NOTIFY_ATTEMPTS: i64 = 5;
const NOTIFY_INTERVAL: Duration = Duration::from_millis(350);

fn file_matches_keywords(file_name: &str, keywords: &[String]) -> bool {
    if keywords.is_empty() {
        return true;
    }

    let file_name = file_name.to_lowercase();
    keywords
        .iter()
        .map(|keyword| keyword.trim().to_lowercase())
        .any(|keyword| !keyword.is_empty() && file_name.contains(&keyword))
}

#[cfg(test)]
mod tests {
    use super::file_matches_keywords;
    use crate::jimaku::{Entry, JimakuClient};
    use chrono::Utc;

    #[test]
    fn empty_file_keyword_does_not_match_every_file() {
        assert!(!file_matches_keywords("release.NF.srt", &[String::new()]));
    }

    #[test]
    fn config_subscription_match_requires_config_filters() {
        let entry = Entry {
            id: 11786,
            name: "Tensei Shitara Slime Datta Ken 4th Season".to_string(),
            english_name: Some("That Time I Got Reincarnated as a Slime Season 4".to_string()),
            japanese_name: None,
            anilist_id: Some(999999),
            tmdb_id: None,
            last_modified: Utc::now(),
            flags: serde_json::json!({}),
            notes: None,
            creator_id: None,
        };
        let config_anilist_ids = Vec::new();
        let config_keywords = Vec::new();
        let config_match = if config_anilist_ids.is_empty() && config_keywords.is_empty() {
            false
        } else {
            JimakuClient::matches_subscription(&entry, &config_anilist_ids, &config_keywords)
        };

        assert!(!config_match);
    }
}
