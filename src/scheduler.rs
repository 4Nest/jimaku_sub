use crate::{
    anilist::AnilistClient,
    bot::text::{channel_caption, channel_link_card, ChannelSubtitle},
    config::Config,
    database::Database,
    downloader::Downloader,
    jimaku::{Entry, FileEntry, JimakuClient},
    telegram::{ChannelNotifier, NewSubtitle, TelegramNotifier},
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::sync::Arc;
use tokio::time::{interval, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

pub struct Scheduler {
    config: Config,
    jimaku: JimakuClient,
    db: Arc<Database>,
    notifier: TelegramNotifier,
    channel: Option<ChannelNotifier>,
    anilist: AnilistClient,
    downloader: Downloader,
}

/// 一个 entry 命中订阅后聚合出的处理策略
struct EntryPolicy {
    keyword_filter: Option<Vec<String>>,
    /// 命中订阅全部被静音时为 false：静默记录，不推送
    notify: bool,
    auto_download: bool,
    /// 通知后把字幕文件直接发到聊天
    send_file: bool,
}

impl Scheduler {
    pub fn new(
        config: Config,
        jimaku: JimakuClient,
        db: Arc<Database>,
        notifier: TelegramNotifier,
        channel: Option<ChannelNotifier>,
    ) -> Self {
        let downloader = Downloader::new(&config.download.download_path);

        Self {
            config,
            jimaku,
            db,
            notifier,
            channel,
            anilist: AnilistClient::new(),
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

        // 频道推送：写入/读取基线时间，并先独立重试上轮遗留的占位行
        let channel_baseline = if self.channel.is_some() {
            match self.db.ensure_channel_enabled_at(Utc::now()).await {
                Ok(baseline) => {
                    if let Err(e) = self.retry_channel_pending().await {
                        error!("Failed to retry pending channel files: {}", e);
                    }
                    Some(baseline)
                }
                Err(e) => {
                    error!("Failed to init channel baseline: {}", e);
                    None
                }
            }
        } else {
            None
        };

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

            let sub_matched = is_global || config_match || !matching_dynamic_subs.is_empty();
            if !sub_matched && self.channel.is_none() {
                debug!("Entry {} does not match subscription, skipping", entry.id);
                continue;
            }

            // files 每轮每 entry 只拉取一次，频道推送与订阅推送共用
            let files = match self.jimaku.get_entry_files(entry.id).await {
                Ok(files) => files,
                Err(e) => {
                    error!("Failed to get files for entry {}: {}", entry.id, e);
                    continue;
                }
            };
            if files.is_empty() {
                continue;
            }

            // 频道全量推送（先于订阅路径，不走订阅匹配）
            if let (Some(_), Some(baseline)) = (&self.channel, channel_baseline) {
                self.process_channel_entry(&entry, &files, baseline).await;
            }

            if !sub_matched {
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

            // 全局/配置订阅命中：沿用全局策略；动态订阅命中：聚合订阅级策略
            let policy = if is_global || config_match {
                EntryPolicy {
                    keyword_filter,
                    notify: true,
                    auto_download: self.config.download.enabled,
                    send_file: false,
                }
            } else {
                let explicit_download: Vec<bool> = matching_dynamic_subs
                    .iter()
                    .filter_map(|sub| sub.auto_download)
                    .collect();
                let auto_download = if explicit_download.is_empty() {
                    self.config.download.enabled
                } else {
                    explicit_download.iter().any(|enabled| *enabled)
                };
                EntryPolicy {
                    keyword_filter,
                    notify: matching_dynamic_subs.iter().any(|sub| !sub.muted),
                    auto_download,
                    send_file: matching_dynamic_subs.iter().any(|sub| sub.send_file),
                }
            };

            if let Err(e) = self.process_entry(&entry, files, &policy).await {
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

    async fn process_entry(
        &self,
        entry: &Entry,
        files: Vec<FileEntry>,
        policy: &EntryPolicy,
    ) -> Result<()> {
        for file in files {
            if let Some(keywords) = policy.keyword_filter.as_deref() {
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
            let mut file_path = None;

            if policy.auto_download {
                match self
                    .downloader
                    .download_subtitle(&entry.name, &file.name, &file.url)
                    .await
                {
                    Ok(path) => {
                        downloaded = true;
                        file_path = Some(path);
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

            // 订阅被静音：静默记录，不推送（避免解除静音后补推刷屏）
            if !policy.notify {
                debug!("Subscription muted, silently recording {}", file.name);
                self.db.mark_notified(file_id).await?;
                continue;
            }

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

                    if policy.send_file {
                        self.send_subtitle_file(entry, &file, file_path).await;
                    }
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

    // ---------- 频道全量推送 ----------

    /// 频道推送一个 entry 的新文件：基线前的旧文件静默标记，新文件发文件+Caption
    async fn process_channel_entry(
        &self,
        entry: &Entry,
        files: &[FileEntry],
        baseline: DateTime<Utc>,
    ) {
        // 分流：已知/死信跳过，基线前的静默标记，其余为待推送新文件
        let mut new_files = Vec::new();
        for file in files {
            match channel_file_action(
                self.db.is_channel_pushed(&file.url).await.unwrap_or(true),
                file.last_modified,
                baseline,
            ) {
                ChannelAction::Skip => continue,
                ChannelAction::Silent => {
                    if let Err(e) = self
                        .db
                        .mark_channel_silent(
                            entry.id,
                            &entry.name,
                            &file.name,
                            &file.url,
                            Some(file.size),
                        )
                        .await
                    {
                        warn!("Failed to mark channel file silent {}: {}", file.name, e);
                    }
                }
                ChannelAction::Send => new_files.push(file),
            }
        }
        if new_files.is_empty() {
            return;
        }

        // 确认有新文件要发才解析罗马音（惰性，不浪费 AniList 配额）
        let romaji = self.resolve_romaji(entry).await;
        let channel = self.channel.as_ref().expect("channel notifier missing");

        for file in new_files {
            let row_id = match self
                .db
                .upsert_channel_pending(
                    entry.id,
                    &entry.name,
                    &file.name,
                    &file.url,
                    Some(file.size),
                )
                .await
            {
                Ok(id) => id,
                Err(e) => {
                    warn!("Failed to upsert channel pending {}: {}", file.name, e);
                    continue;
                }
            };

            let sub = ChannelSubtitle {
                japanese_name: entry.japanese_name.as_deref(),
                english_name: entry.english_name.as_deref(),
                romaji: &romaji,
                file_name: &file.name,
                file_size: file.size,
                file_url: &file.url,
                entry_id: entry.id,
            };

            let result = if file.size > MAX_DOCUMENT_SIZE {
                channel.send_text_card(&channel_link_card(&sub)).await
            } else {
                match self
                    .downloader
                    .download_subtitle(&entry.name, &file.name, &file.url)
                    .await
                {
                    Ok(path) => {
                        channel
                            .send_subtitle_document(&path, &channel_caption(&sub))
                            .await
                    }
                    Err(e) => Err(e),
                }
            };

            match result {
                Ok(_) => {
                    if let Err(e) = self.db.mark_channel_pushed(row_id).await {
                        warn!("Failed to mark channel pushed {}: {}", file.name, e);
                    }
                    info!("Channel pushed: {}", file.name);
                }
                Err(e) => {
                    match self
                        .db
                        .mark_channel_failed(row_id, MAX_CHANNEL_ATTEMPTS)
                        .await
                    {
                        Ok(attempts) => {
                            error!(
                                "Failed to push {} to channel (attempt {}/{}): {}",
                                file.name, attempts, MAX_CHANNEL_ATTEMPTS, e
                            );
                            if attempts >= MAX_CHANNEL_ATTEMPTS {
                                let _ = self
                                    .notifier
                                    .send_message(format!(
                                        "⚠️ 频道推送重试 {} 次仍失败，已放弃: <code>{}</code>",
                                        attempts, file.name
                                    ))
                                    .await;
                            }
                        }
                        Err(db_err) => {
                            error!("Failed to record channel failure {}: {}", file.name, db_err)
                        }
                    }
                }
            }

            tokio::time::sleep(CHANNEL_SEND_INTERVAL).await;
        }
    }

    /// 独立重试上轮遗留的频道推送占位行（不依赖 entry 再次出现在 feed）
    async fn retry_channel_pending(&self) -> Result<()> {
        let pending = self.db.list_channel_pending().await?;
        if pending.is_empty() {
            return Ok(());
        }
        info!("Retrying {} pending channel files", pending.len());

        // 按 entry 分组取作品名，避免重复请求 jimaku
        let mut entry_cache: std::collections::HashMap<i64, Option<Entry>> =
            std::collections::HashMap::new();
        let channel = self.channel.as_ref().expect("channel notifier missing");

        for file in pending {
            let entry = match entry_cache.get(&file.entry_id) {
                Some(cached) => cached.as_ref(),
                None => {
                    let fetched = self.jimaku.get_entry_by_id(file.entry_id).await.ok();
                    entry_cache.insert(file.entry_id, fetched);
                    entry_cache[&file.entry_id].as_ref()
                }
            };

            let (ja, en, romaji) = match entry {
                Some(e) => (
                    e.japanese_name.clone(),
                    e.english_name.clone(),
                    self.resolve_romaji(e).await,
                ),
                None => (None, None, file.entry_name.clone()),
            };

            let sub = ChannelSubtitle {
                japanese_name: ja.as_deref(),
                english_name: en.as_deref(),
                romaji: &romaji,
                file_name: &file.file_name,
                file_size: file.file_size.unwrap_or(0),
                file_url: &file.file_url,
                entry_id: file.entry_id,
            };

            let result = if file.file_size.unwrap_or(0) > MAX_DOCUMENT_SIZE {
                channel.send_text_card(&channel_link_card(&sub)).await
            } else {
                match self
                    .downloader
                    .download_subtitle(&file.entry_name, &file.file_name, &file.file_url)
                    .await
                {
                    Ok(path) => {
                        channel
                            .send_subtitle_document(&path, &channel_caption(&sub))
                            .await
                    }
                    Err(e) => Err(e),
                }
            };

            match result {
                Ok(_) => {
                    self.db.mark_channel_pushed(file.id).await?;
                    info!("Channel retry succeeded: {}", file.file_name);
                }
                Err(e) => {
                    let attempts = self
                        .db
                        .mark_channel_failed(file.id, MAX_CHANNEL_ATTEMPTS)
                        .await?;
                    error!(
                        "Channel retry failed for {} (attempt {}/{}): {}",
                        file.file_name, attempts, MAX_CHANNEL_ATTEMPTS, e
                    );
                    if attempts >= MAX_CHANNEL_ATTEMPTS {
                        let _ = self
                            .notifier
                            .send_message(format!(
                                "⚠️ 频道推送重试 {} 次仍失败，已放弃: <code>{}</code>",
                                attempts, file.file_name
                            ))
                            .await;
                    }
                }
            }

            tokio::time::sleep(CHANNEL_SEND_INTERVAL).await;
        }
        Ok(())
    }

    /// 解析作品罗马音：缓存 → AniList API → 降级 entry.name（本身即罗马音）
    async fn resolve_romaji(&self, entry: &Entry) -> String {
        if let Some(anilist_id) = entry.anilist_id {
            match self.db.get_cached_romaji(anilist_id).await {
                Ok(Some(Some(cached))) => return cached,
                Ok(Some(None)) => debug!("AniList negative cache hit for {}", anilist_id),
                Ok(None) => {
                    let romaji = self.anilist.get_romaji(anilist_id).await;
                    if let Err(e) = self.db.cache_romaji(anilist_id, romaji.as_deref()).await {
                        warn!("Failed to cache romaji for {}: {}", anilist_id, e);
                    }
                    if let Some(romaji) = romaji {
                        return romaji;
                    }
                }
                Err(e) => warn!("Failed to read romaji cache: {}", e),
            }
        }
        entry.name.clone()
    }

    /// 通知后把字幕文件作为文档发送到聊天（订阅开启 send_file 时）
    async fn send_subtitle_file(
        &self,
        entry: &Entry,
        file: &crate::jimaku::FileEntry,
        downloaded_path: Option<std::path::PathBuf>,
    ) {
        let path = match downloaded_path {
            Some(path) => Some(path),
            None => match self
                .downloader
                .download_subtitle(&entry.name, &file.name, &file.url)
                .await
            {
                Ok(path) => {
                    let _ = self
                        .db
                        .record_file(
                            entry.id,
                            &entry.name,
                            &file.name,
                            &file.url,
                            Some(file.size),
                            true,
                        )
                        .await;
                    Some(path)
                }
                Err(e) => {
                    warn!("Failed to download {} for sending: {}", file.name, e);
                    None
                }
            },
        };

        let Some(path) = path else {
            let _ = self
                .notifier
                .send_message(format!(
                    "⚠️ 字幕文件下载失败，无法发送: <code>{}</code>",
                    file.name
                ))
                .await;
            return;
        };

        if let Err(e) = self.notifier.send_document(&path).await {
            error!("Failed to send subtitle file {}: {}", file.name, e);
        }
    }
}

const MAX_NOTIFY_ATTEMPTS: i64 = 5;
const NOTIFY_INTERVAL: Duration = Duration::from_millis(350);

// ---------- 频道推送 ----------
const MAX_CHANNEL_ATTEMPTS: i64 = 5;
/// 频道消息限制比私聊严（约 20 条/分钟），发送间隔取 1s
const CHANNEL_SEND_INTERVAL: Duration = Duration::from_secs(1);
/// Telegram bot 本地上传上限 50MB，jimaku 自报 size 可能有出入，取保守值
const MAX_DOCUMENT_SIZE: i64 = 48 * 1024 * 1024;

/// 频道推送对单个文件的处理动作
#[derive(Debug, PartialEq, Eq)]
enum ChannelAction {
    /// 已推送过或已死信：跳过
    Skip,
    /// 基线前的 backlog：静默标记，不发频道
    Silent,
    /// 新文件：走推送流程
    Send,
}

fn channel_file_action(
    url_known: bool,
    file_modified: DateTime<Utc>,
    baseline: DateTime<Utc>,
) -> ChannelAction {
    if url_known {
        return ChannelAction::Skip;
    }
    if file_modified < baseline {
        return ChannelAction::Silent;
    }
    ChannelAction::Send
}

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
    use super::{channel_file_action, file_matches_keywords, ChannelAction};
    use crate::jimaku::{Entry, JimakuClient};
    use chrono::{Duration, Utc};

    #[test]
    fn channel_action_matrix() {
        let baseline = Utc::now();
        let before = baseline - Duration::hours(1);
        let after = baseline + Duration::hours(1);

        // 已知 url：一律跳过（无论新旧）
        assert_eq!(
            channel_file_action(true, after, baseline),
            ChannelAction::Skip
        );
        assert_eq!(
            channel_file_action(true, before, baseline),
            ChannelAction::Skip
        );
        // 未知 url：基线前静默，基线后推送
        assert_eq!(
            channel_file_action(false, before, baseline),
            ChannelAction::Silent
        );
        assert_eq!(
            channel_file_action(false, after, baseline),
            ChannelAction::Send
        );
    }

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
