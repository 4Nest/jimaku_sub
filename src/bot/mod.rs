//! Telegram Bot 交互层：命令、按钮回调与消息模板

pub mod callbacks;
pub mod commands;
pub mod keyboards;
pub mod text;

use std::sync::Arc;

use anyhow::Result;
use teloxide::types::ChatId;
use tracing::error;

use crate::config::Config;
use crate::database::Database;
use crate::jimaku::JimakuClient;
use crate::scheduler::Scheduler;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Database>,
    pub scheduler: Arc<Scheduler>,
    pub config: Config,
    pub jimaku: Arc<JimakuClient>,
    pub allowed_chat_id: ChatId,
}

impl AppState {
    /// 校验命令/回调来源是否为白名单 chat
    pub fn is_allowed(&self, chat_id: ChatId) -> bool {
        chat_id == self.allowed_chat_id
    }
}

/// 启动时回填订阅标题（best-effort，失败不影响启动）
pub async fn backfill_subscription_titles(state: &AppState) -> Result<()> {
    let subs = state.db.list_subscriptions().await?;
    for sub in subs {
        if let Ok(entry) = state.jimaku.get_entry_by_id(sub.entry_id).await {
            state
                .db
                .update_subscription_title(sub.entry_id, &text::display_entry_title(&entry))
                .await?;
        }
    }
    Ok(())
}

pub async fn backfill_or_log(state: &AppState) {
    if let Err(e) = backfill_subscription_titles(state).await {
        error!("Failed to backfill subscription titles: {}", e);
    }
}
