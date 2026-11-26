//! Telegram Bot 交互层：命令、按钮回调与消息模板

pub mod callbacks;
pub mod commands;
pub mod keyboards;
pub mod text;

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use teloxide::types::ChatId;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::config::Config;
use crate::database::Database;
use crate::downloader::Downloader;
use crate::goindex::GoIndexClient;
use crate::jimaku::JimakuClient;
use crate::scheduler::Scheduler;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Database>,
    pub scheduler: Arc<Scheduler>,
    pub config: Config,
    pub jimaku: Arc<JimakuClient>,
    pub goindex: Option<Arc<GoIndexClient>>,
    pub downloader: Arc<Downloader>,
    pub allowed_chat_id: ChatId,
    pub cancel: CancellationToken,
}

impl AppState {
    /// 校验命令/回调来源是否为白名单 chat
    pub fn is_allowed(&self, chat_id: ChatId) -> bool {
        chat_id == self.allowed_chat_id
    }
}

/// 后台按需回填缺失标题的订阅（已存在标题的不重复请求外部 API，不阻塞服务启动）
pub fn spawn_backfill_titles(state: AppState) {
    tokio::spawn(async move {
        if let Err(e) = backfill_subscription_titles(&state).await {
            warn!("Failed to backfill subscription titles: {}", e);
        }
    });
}

async fn backfill_subscription_titles(state: &AppState) -> Result<()> {
    let subs = state.db.list_subscriptions().await?;
    let missing: Vec<i64> = subs
        .into_iter()
        .filter(|sub| sub.title.as_deref().unwrap_or("").trim().is_empty())
        .map(|sub| sub.entry_id)
        .collect();

    for entry_id in missing {
        if state.cancel.is_cancelled() {
            break;
        }
        match state.jimaku.get_entry_by_id(entry_id).await {
            Ok(entry) => {
                if let Err(e) = state
                    .db
                    .update_subscription_title(entry_id, &text::display_entry_title(&entry))
                    .await
                {
                    warn!(
                        "Failed to update title for subscription {}: {}",
                        entry_id, e
                    );
                }
            }
            Err(e) => {
                warn!(
                    "Failed to fetch entry {} for title backfill: {}",
                    entry_id, e
                );
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            _ = state.cancel.cancelled() => break,
        }
    }
    Ok(())
}
