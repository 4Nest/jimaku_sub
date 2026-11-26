use crate::{
    bot::text::{channel_caption, channel_link_card, html_escape, ChannelSubtitle},
    config::Config,
    database::{Database, Subscription},
    downloader::{Downloader, FetchedSubtitle},
    goindex::{GoIndexClient, GoIndexFile, GOINDEX_ENTRY_ID},
    jimaku::{Entry, FileEntry, JimakuClient},
    telegram::{ChannelNotifier, DocumentSource, NewSubtitle, SubtitleSource, TelegramNotifier},
};
use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use std::{collections::HashMap, sync::Arc};
use tokio::time::{interval, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

pub struct Scheduler {
    config: Config,
    jimaku: Arc<JimakuClient>,
    goindex: Option<Arc<GoIndexClient>>,
    db: Arc<Database>,
    notifier: TelegramNotifier,
    channel: Option<ChannelNotifier>,
    downloader: Arc<Downloader>,
    check_lock: tokio::sync::Mutex<()>,
    last_log_cleanup: std::sync::Mutex<Option<NaiveDate>>,
    sub_entry_cache: tokio::sync::Mutex<HashMap<i64, Entry>>,
}

/// 一个 entry 命中订阅后聚合出的处理策略
#[derive(Debug, PartialEq, Eq)]
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
        jimaku: Arc<JimakuClient>,
        goindex: Option<Arc<GoIndexClient>>,
        db: Arc<Database>,
        downloader: Arc<Downloader>,
        notifier: TelegramNotifier,
        channel: Option<ChannelNotifier>,
    ) -> Self {
        Self {
            config,
            jimaku,
            goindex,
            db,
            notifier,
            channel,
            downloader,
            check_lock: tokio::sync::Mutex::new(()),
            last_log_cleanup: std::sync::Mutex::new(None),
            sub_entry_cache: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    pub async fn run(self: Arc<Self>, cancel: CancellationToken) -> Result<()> {
        let mut ticker = interval(Duration::from_secs(self.config.scheduler.interval_seconds));
        // Consume the immediate first tick so the manual initial check doesn't double-run.
        ticker.tick().await;

        // 启动即写一次心跳，避免首轮积压推送耗时较长导致容器启动期被标为 unhealthy
        self.write_heartbeat().await;

        if let Err(e) = self.check_scheduled(&cancel).await {
            error!("Initial check failed: {}", e);
        }

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    self.maybe_cleanup_logs();
                    if let Err(e) = self.check_scheduled(&cancel).await {
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

    /// 定时任务触发检查（串行等待锁）
    async fn check_scheduled(&self, cancel: &CancellationToken) -> Result<()> {
        let _guard = self.check_lock.lock().await;
        if cancel.is_cancelled() {
            return Ok(());
        }
        self.check_inner(cancel).await
    }

    /// `/checknow` 手动触发检查：若已有检查在跑立即返回 `Ok(false)`，避免并发重入与重复推送
    pub async fn try_check_now(&self, cancel: &CancellationToken) -> Result<bool> {
        let Ok(_guard) = self.check_lock.try_lock() else {
            return Ok(false);
        };
        self.check_inner(cancel).await?;
        Ok(true)
    }

    pub fn is_checking(&self) -> bool {
        self.check_lock.try_lock().is_err()
    }

    fn maybe_cleanup_logs(&self) {
        let today = Utc::now().date_naive();
        if let Ok(mut guard) = self.last_log_cleanup.lock() {
            if *guard == Some(today) {
                return;
            }
            *guard = Some(today);
        }
        crate::cleanup_old_logs(&self.config.logging.dir, self.config.logging.retention_days);
    }

    async fn check_inner(&self, cancel: &CancellationToken) -> Result<()> {
        info!("Starting subtitle check...");

        let dynamic_subs = self.db.list_subscriptions().await?;

        // 1) 独立重试上轮遗留的频道推送占位行
        let channel_baseline = if self.channel.is_some() {
            match self.db.ensure_channel_enabled_at(Utc::now()).await {
                Ok(baseline) => {
                    if let Err(e) = self.retry_channel_pending(cancel).await {
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

        if cancel.is_cancelled() {
            return Ok(());
        }

        // 2) 独立重试上轮遗留的私聊通知占位行（notified=0）
        if let Err(e) = self
            .retry_pending_notifications(&dynamic_subs, cancel)
            .await
        {
            error!("Failed to retry pending notifications: {}", e);
        }

        if cancel.is_cancelled() {
            return Ok(());
        }

        // 3) Jimaku 轮询（独立容错，不阻断 GoIndex 同步）
        let jimaku_res = self
            .check_jimaku(channel_baseline, &dynamic_subs, cancel)
            .await;
        if let Err(ref e) = jimaku_res {
            error!("Jimaku check failed: {}", e);
        }

        if cancel.is_cancelled() {
            return Ok(());
        }

        // 4) GoIndex 放送字幕站轮询
        let goindex_res = if let Some(ref goindex) = self.goindex {
            let res = self.check_goindex(goindex, &dynamic_subs, cancel).await;
            if let Err(ref e) = res {
                error!("GoIndex check failed: {}", e);
            }
            res
        } else {
            Ok(())
        };

        self.write_heartbeat().await;
        jimaku_res?;
        goindex_res?;
        info!("Subtitle check completed");
        Ok(())
    }

    async fn check_jimaku(
        &self,
        channel_baseline: Option<DateTime<Utc>>,
        dynamic_subs: &[Subscription],
        cancel: &CancellationToken,
    ) -> Result<()> {
        let last_check = self.db.get_last_check_time().await?;
        let now = Utc::now().timestamp();

        // 首次运行：检查最近 1 小时的条目
        let after = last_check.unwrap_or(now - 3600);
        debug!("Checking Jimaku entries after timestamp: {}", after);

        let entries = match self.search_entries_with_retry(after, cancel).await {
            Ok(Some(entries)) => entries,
            Ok(None) => return Ok(()),
            Err(e) => return Err(e),
        };
        info!("Found {} Jimaku entries since last check", entries.len());

        let mut had_entry_error = false;

        for entry in entries {
            if cancel.is_cancelled() {
                info!("Check interrupted by shutdown; keeping previous last_check_time");
                return Ok(());
            }

            let policy = self.resolve_entry_policy(&entry, dynamic_subs);
            if policy.is_none() && self.channel.is_none() {
                debug!("Entry {} does not match subscription, skipping", entry.id);
                continue;
            }

            // files 每轮每 entry 只拉取一次，频道推送与订阅推送共用
            let files = match self.jimaku.get_entry_files(entry.id).await {
                Ok(files) => files,
                Err(e) => {
                    error!("Failed to get files for entry {}: {}", entry.id, e);
                    had_entry_error = true;
                    continue;
                }
            };
            if files.is_empty() {
                continue;
            }

            let persist_for_channel =
                self.config.download.enabled || policy.as_ref().is_some_and(|p| p.auto_download);

            // 频道全量推送（先于订阅路径，不走订阅匹配）
            if let (Some(_), Some(baseline)) = (&self.channel, channel_baseline) {
                self.process_channel_entry(&entry, &files, baseline, persist_for_channel, cancel)
                    .await;
            }

            if cancel.is_cancelled() {
                info!("Check interrupted by shutdown; keeping previous last_check_time");
                return Ok(());
            }

            if let Some(policy) = policy {
                if let Err(e) = self.process_entry(&entry, files, &policy, cancel).await {
                    error!("Failed to process entry {}: {}", entry.id, e);
                    had_entry_error = true;
                }
            }

            self.write_heartbeat().await;
        }

        if cancel.is_cancelled() {
            return Ok(());
        }

        if had_entry_error {
            warn!(
                "One or more entries failed to process; keeping previous last_check_time for retry"
            );
        } else {
            self.db.set_last_check_time(now).await?;
        }

        Ok(())
    }

    async fn check_goindex(
        &self,
        goindex: &GoIndexClient,
        dynamic_subs: &[Subscription],
        cancel: &CancellationToken,
    ) -> Result<()> {
        let has_sub_filters =
            !self.config.subscription.name_keywords.is_empty() || !dynamic_subs.is_empty();
        if self.channel.is_none() && !has_sub_filters {
            debug!(
                "GoIndex skipped: neither channel push nor keyword/dynamic subscriptions active"
            );
            return Ok(());
        }

        let files = match self.list_goindex_with_retry(goindex, cancel).await {
            Ok(Some(files)) => files,
            Ok(None) => return Ok(()),
            Err(e) => return Err(e),
        };
        info!("Listed {} subtitle files from GoIndex", files.len());
        if files.is_empty() {
            return Ok(());
        }

        let sub_entries = self.refresh_sub_entry_cache(dynamic_subs, cancel).await;
        if cancel.is_cancelled() {
            return Ok(());
        }

        // 1) 全量频道推送（包含首次启动时的站内现有存量，后续通过 file_url + file_name 双重去重，避免目录归档重推）
        if let Some(ref channel) = self.channel {
            match (
                self.db.get_handled_channel_urls(GOINDEX_ENTRY_ID).await,
                self.db
                    .get_handled_channel_file_names(GOINDEX_ENTRY_ID)
                    .await,
            ) {
                (Ok(handled_urls), Ok(mut handled_names)) => {
                    for file in &files {
                        if cancel.is_cancelled() {
                            return Ok(());
                        }
                        if handled_urls.contains(&file.url) || handled_names.contains(&file.name) {
                            continue;
                        }

                        let matched_policy = resolve_goindex_policy(
                            &self.config.subscription.name_keywords,
                            self.config.download.enabled,
                            &file.name,
                            &file.program_title,
                            dynamic_subs,
                            &sub_entries,
                        );
                        let persist_to_disk = self.config.download.enabled
                            || matched_policy.as_ref().is_some_and(|p| p.auto_download);

                        let row_id = match self
                            .db
                            .upsert_channel_pending(
                                GOINDEX_ENTRY_ID,
                                &file.program_title,
                                &file.name,
                                &file.url,
                                Some(file.size),
                            )
                            .await
                        {
                            Ok(id) => id,
                            Err(e) => {
                                warn!(
                                    "Failed to upsert GoIndex channel pending {}: {}",
                                    file.name, e
                                );
                                continue;
                            }
                        };

                        let sub = ChannelSubtitle {
                            japanese_name: Some(&file.program_title),
                            file_name: &file.name,
                            file_size: file.size,
                            file_url: &file.url,
                            source: SubtitleSource::GoIndex {
                                site_url: goindex.base_url(),
                            },
                            anilist_id: matched_anilist_id(
                                &file.name,
                                &file.program_title,
                                dynamic_subs,
                                &sub_entries,
                            ),
                            file_modified: file.modified_time,
                        };

                        let result = self
                            .deliver_to_channel(
                                channel,
                                &file.program_title,
                                &file.name,
                                &file.url,
                                file.size,
                                persist_to_disk,
                                &sub,
                            )
                            .await;

                        handled_names.insert(file.name.clone());
                        self.handle_channel_result(row_id, &file.name, result).await;
                        self.write_heartbeat().await;

                        if !cancellable_sleep(CHANNEL_SEND_INTERVAL, cancel).await {
                            return Ok(());
                        }
                    }
                }
                (Err(e), _) | (_, Err(e)) => {
                    warn!("Failed to load GoIndex handled channel records: {}", e);
                }
            }
        }

        if cancel.is_cancelled() || !has_sub_filters {
            return Ok(());
        }

        // 2) 关键词与动态订阅匹配 → 私聊通知 / 自动下载（同样按 file_url + file_name 双重去重）
        let handled_notified = self.db.get_handled_notified_urls(GOINDEX_ENTRY_ID).await?;
        let mut handled_notified_names = self
            .db
            .get_handled_notified_file_names(GOINDEX_ENTRY_ID)
            .await?;
        for file in &files {
            if cancel.is_cancelled() {
                return Ok(());
            }
            if handled_notified.contains(&file.url) || handled_notified_names.contains(&file.name) {
                continue;
            }
            let Some(policy) = resolve_goindex_policy(
                &self.config.subscription.name_keywords,
                self.config.download.enabled,
                &file.name,
                &file.program_title,
                dynamic_subs,
                &sub_entries,
            ) else {
                continue;
            };
            handled_notified_names.insert(file.name.clone());

            let mut downloaded = false;
            let mut downloaded_outcome = None;
            if policy.auto_download {
                match self
                    .downloader
                    .download_subtitle_with_outcome(&file.program_title, &file.name, &file.url)
                    .await
                {
                    Ok(outcome) => {
                        downloaded = true;
                        downloaded_outcome = Some(outcome);
                    }
                    Err(e) => {
                        warn!("Failed to download GoIndex subtitle {}: {}", file.name, e);
                    }
                }
            }

            let file_id = self
                .db
                .upsert_pending_file(
                    GOINDEX_ENTRY_ID,
                    &file.program_title,
                    &file.name,
                    &file.url,
                    Some(file.size),
                    downloaded,
                )
                .await?;

            if !policy.notify {
                debug!(
                    "Subscription muted, silently recording GoIndex {}",
                    file.name
                );
                self.db.mark_notified(file_id).await?;
                continue;
            }

            let send_res = self
                .notifier
                .notify_new_subtitle(&NewSubtitle {
                    file_id,
                    entry_name: &file.program_title,
                    english_name: None,
                    japanese_name: None,
                    file_name: &file.name,
                    file_size: file.size,
                    file_url: &file.url,
                    file_modified: file.modified_time,
                    source: SubtitleSource::GoIndex {
                        site_url: goindex.base_url(),
                    },
                    downloaded,
                })
                .await;

            match send_res {
                Ok(_) => {
                    self.db.mark_notified(file_id).await?;
                    if policy.send_file {
                        self.send_subtitle_file(
                            file_id,
                            &file.program_title,
                            &file.name,
                            &file.url,
                            file.size,
                            policy.auto_download,
                            downloaded_outcome.map(|o| (o.path, o.size)),
                        )
                        .await;
                    }
                }
                Err(e) if send_maybe_delivered(&e) => {
                    warn!(
                        "Notification for {} timed out after reaching Telegram, treating as delivered: {}",
                        file.name, e
                    );
                    self.db.mark_notified(file_id).await?;
                }
                Err(e) => {
                    self.record_notify_failure(file_id, &file.name, &e).await?;
                }
            }

            self.write_heartbeat().await;
            if !cancellable_sleep(NOTIFY_INTERVAL, cancel).await {
                return Ok(());
            }
        }

        Ok(())
    }

    async fn refresh_sub_entry_cache(
        &self,
        dynamic_subs: &[Subscription],
        cancel: &CancellationToken,
    ) -> HashMap<i64, Entry> {
        let active_ids: std::collections::HashSet<i64> =
            dynamic_subs.iter().map(|s| s.entry_id).collect();
        let missing: Vec<i64> = {
            let mut cache = self.sub_entry_cache.lock().await;
            cache.retain(|id, _| active_ids.contains(id));
            active_ids
                .iter()
                .copied()
                .filter(|id| *id > 0 && !cache.contains_key(id))
                .collect()
        };

        for entry_id in missing {
            if cancel.is_cancelled() {
                break;
            }
            if let Ok(entry) = self.jimaku.get_entry_by_id(entry_id).await {
                self.sub_entry_cache.lock().await.insert(entry_id, entry);
            }
        }

        self.sub_entry_cache.lock().await.clone()
    }

    async fn list_goindex_with_retry(
        &self,
        goindex: &GoIndexClient,
        cancel: &CancellationToken,
    ) -> Result<Option<Vec<GoIndexFile>>> {
        const MAX_ATTEMPTS: u32 = 3;
        let mut attempt = 0;
        loop {
            attempt += 1;
            match goindex.list_subtitle_files().await {
                Ok(files) => return Ok(Some(files)),
                Err(e) if attempt < MAX_ATTEMPTS => {
                    let backoff = Duration::from_secs(1 << (attempt - 1));
                    warn!(
                        "GoIndex list failed (attempt {}/{}): {}, retrying in {:?}",
                        attempt, MAX_ATTEMPTS, e, backoff
                    );
                    if !cancellable_sleep(backoff, cancel).await {
                        return Ok(None);
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// 计算 entry 命中的订阅策略；未命中任何配置/动态订阅时返回 `None`
    fn resolve_entry_policy(
        &self,
        entry: &Entry,
        dynamic_subs: &[Subscription],
    ) -> Option<EntryPolicy> {
        let config_anilist_ids = &self.config.subscription.anilist_ids;
        let config_keywords = &self.config.subscription.name_keywords;

        let config_match = if config_anilist_ids.is_empty() && config_keywords.is_empty() {
            false
        } else {
            JimakuClient::matches_subscription(entry, config_anilist_ids, config_keywords)
        };

        let matching_dynamic_subs: Vec<&Subscription> = dynamic_subs
            .iter()
            .filter(|sub| entry.id == sub.entry_id)
            .collect();

        if !config_match && matching_dynamic_subs.is_empty() {
            return None;
        }

        let keyword_filter = if config_match
            || matching_dynamic_subs
                .iter()
                .any(|sub| sub.keywords.is_empty())
        {
            None
        } else {
            Some(
                matching_dynamic_subs
                    .iter()
                    .flat_map(|sub| sub.keywords.iter().cloned())
                    .collect(),
            )
        };

        if config_match {
            Some(EntryPolicy {
                keyword_filter,
                notify: true,
                auto_download: self.config.download.enabled,
                send_file: false,
            })
        } else {
            let explicit_download: Vec<bool> = matching_dynamic_subs
                .iter()
                .filter_map(|sub| sub.auto_download)
                .collect();
            let auto_download = if explicit_download.is_empty() {
                self.config.download.enabled
            } else {
                explicit_download.into_iter().any(|enabled| enabled)
            };
            Some(EntryPolicy {
                keyword_filter,
                notify: matching_dynamic_subs.iter().any(|sub| !sub.muted),
                auto_download,
                send_file: matching_dynamic_subs.iter().any(|sub| sub.send_file),
            })
        }
    }

    /// Jimaku 搜索失败时有限重试（指数退避 1s/2s/4s），收到停机信号立即返回 `Ok(None)`
    async fn search_entries_with_retry(
        &self,
        after: i64,
        cancel: &CancellationToken,
    ) -> Result<Option<Vec<Entry>>> {
        const MAX_ATTEMPTS: u32 = 3;
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.jimaku.search_entries(Some(after), None).await {
                Ok(entries) => return Ok(Some(entries)),
                Err(e) if attempt < MAX_ATTEMPTS => {
                    let backoff = Duration::from_secs(1 << (attempt - 1));
                    warn!(
                        "Jimaku search failed (attempt {}/{}): {}, retrying in {:?}",
                        attempt, MAX_ATTEMPTS, e, backoff
                    );
                    if !cancellable_sleep(backoff, cancel).await {
                        return Ok(None);
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// 每轮成功 check 或处理进度推进后刷新心跳文件，供 Docker healthcheck 使用
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
        cancel: &CancellationToken,
    ) -> Result<()> {
        let handled_urls = self.db.get_handled_notified_urls(entry.id).await?;

        for file in files {
            if cancel.is_cancelled() {
                return Ok(());
            }

            if let Some(keywords) = policy.keyword_filter.as_deref() {
                if !file_matches_keywords(&file.name, keywords) {
                    debug!(
                        "File {} does not match subtitle keywords, skipping",
                        file.name
                    );
                    continue;
                }
            }

            if handled_urls.contains(&file.url) {
                debug!("File already handled: {}", file.name);
                continue;
            }

            let mut downloaded = false;
            let mut downloaded_outcome = None;

            if policy.auto_download {
                match self
                    .downloader
                    .download_subtitle_with_outcome(&entry.name, &file.name, &file.url)
                    .await
                {
                    Ok(outcome) => {
                        downloaded = true;
                        downloaded_outcome = Some(outcome);
                    }
                    Err(e) => {
                        warn!("Failed to download {}: {}", file.name, e);
                    }
                }
            }

            // 先占位（notified=0），发送成功才确认，失败留下轮 retry_pending_notifications 重试
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

            let send_res = self
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
                    source: SubtitleSource::Jimaku { entry_id: entry.id },
                    downloaded,
                })
                .await;

            match send_res {
                Ok(_) => {
                    self.db.mark_notified(file_id).await?;
                    if policy.send_file {
                        self.send_subtitle_file(
                            file_id,
                            &entry.name,
                            &file.name,
                            &file.url,
                            file.size,
                            policy.auto_download,
                            downloaded_outcome.map(|o| (o.path, o.size)),
                        )
                        .await;
                    }
                }
                Err(e) if send_maybe_delivered(&e) => {
                    warn!(
                        "Notification for {} timed out after reaching Telegram, treating as delivered: {}",
                        file.name, e
                    );
                    self.db.mark_notified(file_id).await?;
                }
                Err(e) => {
                    self.record_notify_failure(file_id, &file.name, &e).await?;
                }
            }

            if !cancellable_sleep(NOTIFY_INTERVAL, cancel).await {
                return Ok(());
            }
        }

        Ok(())
    }

    /// 独立重试上轮遗留的私聊通知占位行（notified=0），不依赖 entry 再次出现在 feed
    async fn retry_pending_notifications(
        &self,
        dynamic_subs: &[Subscription],
        cancel: &CancellationToken,
    ) -> Result<()> {
        let pending = self.db.list_pending_notifications().await?;
        if pending.is_empty() {
            return Ok(());
        }
        info!("Retrying {} pending subtitle notifications", pending.len());

        let sub_entries = self.refresh_sub_entry_cache(dynamic_subs, cancel).await;
        let mut entry_cache: HashMap<i64, Option<Entry>> = HashMap::new();
        let goindex_site_url = self.config.goindex.base_url.trim().trim_end_matches('/');

        for item in pending {
            if cancel.is_cancelled() {
                return Ok(());
            }

            let (entry, policy, source) = if item.entry_id == GOINDEX_ENTRY_ID {
                let policy = resolve_goindex_policy(
                    &self.config.subscription.name_keywords,
                    self.config.download.enabled,
                    &item.file_name,
                    &item.entry_name,
                    dynamic_subs,
                    &sub_entries,
                );
                (
                    None,
                    policy,
                    SubtitleSource::GoIndex {
                        site_url: goindex_site_url,
                    },
                )
            } else {
                let entry = match entry_cache.get(&item.entry_id) {
                    Some(cached) => cached.as_ref(),
                    None => {
                        let fetched = self.jimaku.get_entry_by_id(item.entry_id).await.ok();
                        entry_cache.insert(item.entry_id, fetched);
                        entry_cache[&item.entry_id].as_ref()
                    }
                };
                let policy = entry.and_then(|e| self.resolve_entry_policy(e, dynamic_subs));
                (
                    entry,
                    policy,
                    SubtitleSource::Jimaku {
                        entry_id: item.entry_id,
                    },
                )
            };

            // 如果在此期间用户已退订或将订阅设为静音，直接结案不再推送
            if policy.as_ref().is_some_and(|p| !p.notify) {
                self.db.mark_notified(item.id).await?;
                continue;
            }

            let res = self
                .notifier
                .notify_new_subtitle(&NewSubtitle {
                    file_id: item.id,
                    entry_name: &item.entry_name,
                    english_name: entry.and_then(|e| e.english_name.as_deref()),
                    japanese_name: entry.and_then(|e| e.japanese_name.as_deref()),
                    file_name: &item.file_name,
                    file_size: item.file_size.unwrap_or(0),
                    file_url: &item.file_url,
                    file_modified: Utc::now(),
                    source,
                    downloaded: item.downloaded,
                })
                .await;

            match res {
                Ok(_) => {
                    self.db.mark_notified(item.id).await?;
                    info!("Notification retry succeeded: {}", item.file_name);
                    if let Some(p) = policy.filter(|p| p.send_file) {
                        self.send_subtitle_file(
                            item.id,
                            &item.entry_name,
                            &item.file_name,
                            &item.file_url,
                            item.file_size.unwrap_or(0),
                            p.auto_download,
                            None,
                        )
                        .await;
                    }
                }
                Err(e) if send_maybe_delivered(&e) => {
                    warn!(
                        "Retry notification for {} timed out after reaching Telegram, treating as delivered: {}",
                        item.file_name, e
                    );
                    self.db.mark_notified(item.id).await?;
                }
                Err(e) => {
                    self.record_notify_failure(item.id, &item.file_name, &e)
                        .await?;
                }
            }

            if !cancellable_sleep(NOTIFY_INTERVAL, cancel).await {
                return Ok(());
            }
        }
        Ok(())
    }

    async fn record_notify_failure(
        &self,
        file_id: i64,
        file_name: &str,
        err: &anyhow::Error,
    ) -> Result<()> {
        let attempts = self
            .db
            .mark_notify_failed(file_id, MAX_NOTIFY_ATTEMPTS)
            .await?;
        error!(
            "Failed to send notification for {} (attempt {}/{}): {}",
            file_name, attempts, MAX_NOTIFY_ATTEMPTS, err
        );
        if attempts >= MAX_NOTIFY_ATTEMPTS {
            let _ = self
                .notifier
                .send_message(format!(
                    "⚠️ 字幕通知重试 {} 次仍失败，已放弃: <code>{}</code>",
                    attempts,
                    html_escape(file_name)
                ))
                .await;
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
        persist_to_disk: bool,
        cancel: &CancellationToken,
    ) {
        let handled_urls = match self.db.get_handled_channel_urls(entry.id).await {
            Ok(urls) => urls,
            Err(e) => {
                warn!(
                    "Failed to load handled channel URLs for entry {}: {}",
                    entry.id, e
                );
                return;
            }
        };

        let mut new_files = Vec::new();
        for file in files {
            match channel_file_action(
                handled_urls.contains(&file.url),
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

        let channel = self.channel.as_ref().expect("channel notifier missing");

        for file in new_files {
            if cancel.is_cancelled() {
                return;
            }

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
                file_name: &file.name,
                file_size: file.size,
                source: SubtitleSource::Jimaku { entry_id: entry.id },
                file_url: &file.url,
                anilist_id: entry.anilist_id,
                file_modified: file.last_modified,
            };

            let result = self
                .deliver_to_channel(
                    channel,
                    &entry.name,
                    &file.name,
                    &file.url,
                    file.size,
                    persist_to_disk,
                    &sub,
                )
                .await;

            self.handle_channel_result(row_id, &file.name, result).await;

            if !cancellable_sleep(CHANNEL_SEND_INTERVAL, cancel).await {
                return;
            }
        }
    }

    /// 独立重试上轮遗留的频道推送占位行（不依赖 entry 再次出现在 feed）
    async fn retry_channel_pending(&self, cancel: &CancellationToken) -> Result<()> {
        let pending = self.db.list_channel_pending().await?;
        if pending.is_empty() {
            return Ok(());
        }
        info!("Retrying {} pending channel files", pending.len());

        let mut entry_cache: HashMap<i64, Option<Entry>> = HashMap::new();
        let channel = self.channel.as_ref().expect("channel notifier missing");
        let goindex_site_url = self.config.goindex.base_url.trim().trim_end_matches('/');

        for file in pending {
            if cancel.is_cancelled() {
                return Ok(());
            }

            let size = file.file_size.unwrap_or(0);
            let sub = if file.entry_id == GOINDEX_ENTRY_ID {
                ChannelSubtitle {
                    japanese_name: Some(&file.entry_name),
                    file_name: &file.file_name,
                    file_size: size,
                    source: SubtitleSource::GoIndex {
                        site_url: goindex_site_url,
                    },
                    file_url: &file.file_url,
                    anilist_id: None,
                    file_modified: Utc::now(),
                }
            } else {
                let entry = match entry_cache.get(&file.entry_id) {
                    Some(cached) => cached.as_ref(),
                    None => {
                        let fetched = self.jimaku.get_entry_by_id(file.entry_id).await.ok();
                        entry_cache.insert(file.entry_id, fetched);
                        entry_cache[&file.entry_id].as_ref()
                    }
                };
                ChannelSubtitle {
                    japanese_name: entry.and_then(|e| e.japanese_name.as_deref()),
                    file_name: &file.file_name,
                    file_size: size,
                    source: SubtitleSource::Jimaku {
                        entry_id: file.entry_id,
                    },
                    file_url: &file.file_url,
                    anilist_id: entry.and_then(|e| e.anilist_id),
                    file_modified: Utc::now(),
                }
            };

            let mut result = self
                .deliver_to_channel(
                    channel,
                    &file.entry_name,
                    &file.file_name,
                    &file.file_url,
                    size,
                    self.config.download.enabled,
                    &sub,
                )
                .await;

            // 仅在 GoIndex 下载阶段 404（未发出 Telegram 消息）时尝试归档迁移路径恢复
            if file.entry_id == GOINDEX_ENTRY_ID {
                if let (Err(ref err), Some(ref goindex)) = (&result, &self.goindex) {
                    if err.to_string().contains("Download failed with status: 404") {
                        let candidates =
                            goindex.candidate_archive_urls(&file.file_name, &file.file_url);
                        let mut recovered = false;
                        for alt_url in &candidates {
                            let alt_sub = ChannelSubtitle {
                                file_url: alt_url,
                                ..sub
                            };
                            match self
                                .deliver_to_channel(
                                    channel,
                                    &file.entry_name,
                                    &file.file_name,
                                    alt_url,
                                    size,
                                    self.config.download.enabled,
                                    &alt_sub,
                                )
                                .await
                            {
                                Ok(()) => {
                                    let _ = self.db.update_channel_file_url(file.id, alt_url).await;
                                    result = Ok(());
                                    recovered = true;
                                    break;
                                }
                                Err(e)
                                    if e.to_string()
                                        .contains("Download failed with status: 404") =>
                                {
                                    continue;
                                }
                                Err(e) => {
                                    result = Err(e);
                                    recovered = true;
                                    break;
                                }
                            }
                        }
                        if !recovered {
                            if let Ok(Some(found_url)) =
                                goindex.find_file_url_by_name(&file.file_name).await
                            {
                                if found_url != file.file_url && !candidates.contains(&found_url) {
                                    let alt_sub = ChannelSubtitle {
                                        file_url: &found_url,
                                        ..sub
                                    };
                                    result = self
                                        .deliver_to_channel(
                                            channel,
                                            &file.entry_name,
                                            &file.file_name,
                                            &found_url,
                                            size,
                                            self.config.download.enabled,
                                            &alt_sub,
                                        )
                                        .await;
                                    if result.is_ok() {
                                        let _ = self
                                            .db
                                            .update_channel_file_url(file.id, &found_url)
                                            .await;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            self.handle_channel_result(file.id, &file.file_name, result)
                .await;
            self.write_heartbeat().await;

            if !cancellable_sleep(CHANNEL_SEND_INTERVAL, cancel).await {
                return Ok(());
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn deliver_to_channel(
        &self,
        channel: &ChannelNotifier,
        entry_name: &str,
        file_name: &str,
        file_url: &str,
        declared_size: i64,
        persist_to_disk: bool,
        sub: &ChannelSubtitle<'_>,
    ) -> Result<()> {
        if declared_size > 0 && (declared_size as u64) > MAX_DOCUMENT_SIZE {
            return channel.send_text_card(&channel_link_card(sub)).await;
        }

        // 若本地已有文件（如订阅已触发下载），直接复用
        let local = self.downloader.local_path(entry_name, file_name);
        if let Ok(meta) = tokio::fs::metadata(&local).await {
            if meta.len() > 0 {
                if meta.len() > MAX_DOCUMENT_SIZE {
                    return channel.send_text_card(&channel_link_card(sub)).await;
                }
                return channel
                    .send_subtitle_document(DocumentSource::Path(&local), &channel_caption(sub))
                    .await;
            }
        }

        if persist_to_disk {
            let outcome = self
                .downloader
                .download_subtitle_with_outcome(entry_name, file_name, file_url)
                .await?;
            if outcome.size > MAX_DOCUMENT_SIZE {
                return channel.send_text_card(&channel_link_card(sub)).await;
            }
            channel
                .send_subtitle_document(DocumentSource::Path(&outcome.path), &channel_caption(sub))
                .await
        } else {
            match self
                .downloader
                .fetch_subtitle_bytes(file_url, MAX_DOCUMENT_SIZE as usize)
                .await?
            {
                FetchedSubtitle::TooLarge => channel.send_text_card(&channel_link_card(sub)).await,
                FetchedSubtitle::Bytes(data) => {
                    channel
                        .send_subtitle_document(
                            DocumentSource::Memory { file_name, data },
                            &channel_caption(sub),
                        )
                        .await
                }
            }
        }
    }

    async fn handle_channel_result(&self, row_id: i64, file_name: &str, result: Result<()>) {
        match result {
            Ok(_) => {
                if let Err(e) = self.db.mark_channel_pushed(row_id).await {
                    warn!("Failed to mark channel pushed {}: {}", file_name, e);
                }
                info!("Channel pushed: {}", file_name);
            }
            Err(e) if send_maybe_delivered(&e) => {
                warn!(
                    "Send of {} timed out after reaching Telegram, treating as delivered: {}",
                    file_name, e
                );
                if let Err(e) = self.db.mark_channel_pushed(row_id).await {
                    warn!("Failed to mark channel pushed {}: {}", file_name, e);
                }
            }
            Err(e) => match self
                .db
                .mark_channel_failed(row_id, MAX_CHANNEL_ATTEMPTS)
                .await
            {
                Ok(attempts) => {
                    error!(
                        "Failed to push {} to channel (attempt {}/{}): {}",
                        file_name, attempts, MAX_CHANNEL_ATTEMPTS, e
                    );
                    if attempts >= MAX_CHANNEL_ATTEMPTS {
                        let _ = self
                            .notifier
                            .send_message(format!(
                                "⚠️ 频道推送重试 {} 次仍失败，已放弃: <code>{}</code>",
                                attempts,
                                html_escape(file_name)
                            ))
                            .await;
                    }
                }
                Err(db_err) => {
                    error!("Failed to record channel failure {}: {}", file_name, db_err);
                }
            },
        }
    }

    /// 通知后把字幕文件作为文档发送到聊天（订阅开启 send_file 时）
    #[allow(clippy::too_many_arguments)]
    async fn send_subtitle_file(
        &self,
        file_id: i64,
        entry_name: &str,
        file_name: &str,
        file_url: &str,
        declared_size: i64,
        persist_to_disk: bool,
        downloaded_file: Option<(std::path::PathBuf, u64)>,
    ) {
        if declared_size > 0 && (declared_size as u64) > MAX_DOCUMENT_SIZE {
            let _ = self
                .notifier
                .send_message(format!(
                    "⚠️ 字幕文件超过 Telegram 发送上限 (48MB)，请从网页下载: <code>{}</code>",
                    html_escape(file_name)
                ))
                .await;
            return;
        }

        if let Some((path, size)) = downloaded_file {
            if size > MAX_DOCUMENT_SIZE {
                let _ = self
                    .notifier
                    .send_message(format!(
                        "⚠️ 字幕文件超过 Telegram 发送上限 (48MB): <code>{}</code>",
                        html_escape(file_name)
                    ))
                    .await;
                return;
            }
            if let Err(e) = self
                .notifier
                .send_document(DocumentSource::Path(&path))
                .await
            {
                error!("Failed to send subtitle file {}: {}", file_name, e);
            }
            return;
        }

        // 检查本地是否已存在
        let local = self.downloader.local_path(entry_name, file_name);
        if let Ok(meta) = tokio::fs::metadata(&local).await {
            if meta.len() > 0 && meta.len() <= MAX_DOCUMENT_SIZE {
                if let Err(e) = self
                    .notifier
                    .send_document(DocumentSource::Path(&local))
                    .await
                {
                    error!("Failed to send subtitle file {}: {}", file_name, e);
                }
                return;
            }
        }

        if persist_to_disk {
            match self
                .downloader
                .download_subtitle_with_outcome(entry_name, file_name, file_url)
                .await
            {
                Ok(outcome) => {
                    let _ = self.db.mark_file_downloaded(file_id).await;
                    if outcome.size > MAX_DOCUMENT_SIZE {
                        let _ = self
                            .notifier
                            .send_message(format!(
                                "⚠️ 字幕文件已下载到本地，但超过 Telegram 发送上限 (48MB): <code>{}</code>",
                                html_escape(file_name)
                            ))
                            .await;
                        return;
                    }
                    if let Err(e) = self
                        .notifier
                        .send_document(DocumentSource::Path(&outcome.path))
                        .await
                    {
                        error!("Failed to send subtitle file {}: {}", file_name, e);
                    }
                }
                Err(e) => {
                    warn!("Failed to download {} for sending: {}", file_name, e);
                    let _ = self
                        .notifier
                        .send_message(format!(
                            "⚠️ 字幕文件下载失败，无法发送: <code>{}</code>",
                            html_escape(file_name)
                        ))
                        .await;
                }
            }
        } else {
            match self
                .downloader
                .fetch_subtitle_bytes(file_url, MAX_DOCUMENT_SIZE as usize)
                .await
            {
                Ok(FetchedSubtitle::Bytes(data)) => {
                    if let Err(e) = self
                        .notifier
                        .send_document(DocumentSource::Memory { file_name, data })
                        .await
                    {
                        error!("Failed to send subtitle file {}: {}", file_name, e);
                    }
                }
                Ok(FetchedSubtitle::TooLarge) => {
                    let _ = self
                        .notifier
                        .send_message(format!(
                            "⚠️ 字幕文件超过 Telegram 发送上限 (48MB)，请从网页下载: <code>{}</code>",
                            html_escape(file_name)
                        ))
                        .await;
                }
                Err(e) => {
                    warn!("Failed to fetch {} for sending: {}", file_name, e);
                    let _ = self
                        .notifier
                        .send_message(format!(
                            "⚠️ 字幕文件下载失败，无法发送: <code>{}</code>",
                            html_escape(file_name)
                        ))
                        .await;
                }
            }
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
pub const MAX_DOCUMENT_SIZE: u64 = 48 * 1024 * 1024;

/// 可响应停机信号的异步 sleep：正常睡完返回 `true`，收到取消信号返回 `false`
async fn cancellable_sleep(duration: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(duration) => true,
        _ = cancel.cancelled() => false,
    }
}

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

/// 发送「疑似已送达」判定：请求已发出（非连接阶段失败）但等响应超时。
/// Telegram API 无幂等键，此时重试会在频道产生重复消息，应改按已送达处理。
fn send_maybe_delivered(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<teloxide::RequestError>(),
            Some(teloxide::RequestError::Network(e)) if e.is_timeout() && !e.is_connect()
        )
    })
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

fn normalize_match_title(s: &str) -> String {
    s.trim()
        .trim_end_matches(['。', '.', '！', '!', '？', '?'])
        .trim()
        .to_lowercase()
}

fn goindex_matches_subscription(
    file_name: &str,
    program_title: &str,
    sub: &Subscription,
    entry: Option<&Entry>,
) -> bool {
    let prog_norm = normalize_match_title(program_title);
    if prog_norm.is_empty() {
        return false;
    }

    let mut candidates: Vec<&str> = Vec::with_capacity(4);
    if let Some(t) = sub.title.as_deref() {
        candidates.push(t);
    }
    if let Some(e) = entry {
        candidates.push(&e.name);
        if let Some(en) = e.english_name.as_deref() {
            candidates.push(en);
        }
        if let Some(ja) = e.japanese_name.as_deref() {
            candidates.push(ja);
        }
    }

    let title_matched = candidates.into_iter().any(|cand| {
        let norm = normalize_match_title(cand);
        norm.chars().count() >= 2 && prog_norm.contains(&norm)
    });
    if !title_matched {
        return false;
    }

    sub.keywords.is_empty() || file_matches_keywords(file_name, &sub.keywords)
}

fn matched_anilist_id(
    file_name: &str,
    program_title: &str,
    dynamic_subs: &[Subscription],
    sub_entries: &HashMap<i64, Entry>,
) -> Option<i32> {
    dynamic_subs.iter().find_map(|sub| {
        let entry = sub_entries.get(&sub.entry_id);
        if goindex_matches_subscription(file_name, program_title, sub, entry) {
            entry.and_then(|e| e.anilist_id)
        } else {
            None
        }
    })
}

/// 计算单个 GoIndex 文件命中的订阅策略（配置关键词 + 动态订阅联合匹配）
fn resolve_goindex_policy(
    config_keywords: &[String],
    download_enabled: bool,
    file_name: &str,
    program_title: &str,
    dynamic_subs: &[Subscription],
    sub_entries: &HashMap<i64, Entry>,
) -> Option<EntryPolicy> {
    let file_lower = file_name.to_lowercase();
    let title_lower = program_title.to_lowercase();

    let config_match = config_keywords
        .iter()
        .map(|kw| kw.trim())
        .filter(|kw| !kw.is_empty())
        .any(|kw| {
            let kw = kw.to_lowercase();
            file_lower.contains(&kw) || title_lower.contains(&kw)
        });

    let matching_dynamic: Vec<&Subscription> = dynamic_subs
        .iter()
        .filter(|sub| {
            let entry = sub_entries.get(&sub.entry_id);
            goindex_matches_subscription(file_name, program_title, sub, entry)
        })
        .collect();

    if !config_match && matching_dynamic.is_empty() {
        return None;
    }

    if config_match {
        Some(EntryPolicy {
            keyword_filter: None,
            notify: true,
            auto_download: download_enabled,
            send_file: false,
        })
    } else {
        let explicit_download: Vec<bool> = matching_dynamic
            .iter()
            .filter_map(|sub| sub.auto_download)
            .collect();
        let auto_download = if explicit_download.is_empty() {
            download_enabled
        } else {
            explicit_download.into_iter().any(|enabled| enabled)
        };
        Some(EntryPolicy {
            keyword_filter: None,
            notify: matching_dynamic.iter().any(|sub| !sub.muted),
            auto_download,
            send_file: matching_dynamic.iter().any(|sub| sub.send_file),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        channel_file_action, file_matches_keywords, resolve_goindex_policy, ChannelAction,
        EntryPolicy,
    };
    use crate::database::Subscription;
    use crate::jimaku::{Entry, JimakuClient};
    use chrono::{Duration, Utc};
    use std::collections::HashMap;

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
    fn send_maybe_delivered_rejects_non_network_errors() {
        // 下载失败等非 Telegram 网络超时错误不能误判为「已送达」
        let err = anyhow::anyhow!("Download failed with status: 404 Not Found");
        assert!(!super::send_maybe_delivered(&err));
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

    #[test]
    fn goindex_policy_matches_config_keywords_and_dynamic_japanese_title_with_release_rule() {
        let sub = Subscription {
            entry_id: 12711,
            title: Some(
                "The Laid-Off Cheat-Granting Mage Enjoys a Second Lease on Life".to_string(),
            ),
            keywords: vec!["AT-X".to_string()],
            created_at: Utc::now(),
            muted: false,
            auto_download: Some(true),
            send_file: true,
        };
        let entry = Entry {
            id: 12711,
            name: "Tsuihou Sareta Cheat Fuyo Majutsushi".to_string(),
            english_name: sub.title.clone(),
            japanese_name: Some(
                "追放されたチート付与魔術師は気ままなセカンドライフを謳歌する。".to_string(),
            ),
            anilist_id: Some(180000),
            tmdb_id: None,
            last_modified: Utc::now(),
            flags: serde_json::json!({}),
            notes: None,
            creator_id: None,
        };
        let entries = HashMap::from([(12711, entry)]);

        // 标题命中但非 AT-X 放送局 → 不触发 AT-X 限定订阅
        assert_eq!(
            resolve_goindex_policy(
                &[],
                false,
                "20261104 0006 [テレビ愛知1] 追放されたチート付与魔術師は気ままなセカンドライフを謳歌する。 #5[字].ass",
                "追放されたチート付与魔術師は気ままなセカンドライフを謳歌する。",
                std::slice::from_ref(&sub),
                &entries,
            ),
            None
        );

        // AT-X 放送局 → 命中动态订阅并继承订阅策略
        assert_eq!(
            resolve_goindex_policy(
                &[],
                false,
                "20261104 2100 [AT-X] 追放されたチート付与魔術師は気ままなセカンドライフを謳歌する #5 [字].ass",
                "追放されたチート付与魔術師は気ままなセカンドライフを謳歌する",
                std::slice::from_ref(&sub),
                &entries,
            ),
            Some(EntryPolicy {
                keyword_filter: None,
                notify: true,
                auto_download: true,
                send_file: true,
            })
        );

        // 配置关键词直接匹配
        assert_eq!(
            resolve_goindex_policy(
                &["千歳くん".to_string()],
                false,
                "20261103 2300 [AT-X] 千歳くんはラムネ瓶のなか #17 [字].ass",
                "千歳くんはラムネ瓶のなか",
                &[],
                &HashMap::new(),
            ),
            Some(EntryPolicy {
                keyword_filter: None,
                notify: true,
                auto_download: false,
                send_file: false,
            })
        );
    }
}
