use anyhow::{Context, Result};
use teloxide::adaptors::throttle::Limits;
use teloxide::adaptors::Throttle;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tracing::info;

/// 通知用 Bot：带限流，自动遵循 Telegram API 速率限制并在 429 时等待重试
pub type ThrottledBot = Throttle<Bot>;

#[derive(Clone)]
pub struct TelegramNotifier {
    bot: ThrottledBot,
    chat_id: ChatId,
}

/// 一条新字幕通知所需的全部信息
pub struct NewSubtitle<'a> {
    pub entry_name: &'a str,
    pub english_name: Option<&'a str>,
    pub japanese_name: Option<&'a str>,
    pub file_name: &'a str,
    pub file_size: i64,
    pub file_url: &'a str,
    pub entry_id: i64,
    pub downloaded: bool,
}

impl TelegramNotifier {
    pub fn new(bot_token: impl Into<String>, chat_id: impl Into<String>) -> Result<Self> {
        let bot = Bot::new(bot_token.into()).throttle(Limits::default());
        let chat_id_str = chat_id.into();
        let chat_id = chat_id_str
            .parse::<i64>()
            .map(ChatId)
            .context("TELEGRAM_CHAT_ID 必须是数字 chat id（可通过 @userinfobot 获取）")?;
        Ok(Self { bot, chat_id })
    }

    pub async fn notify_new_subtitle(&self, sub: &NewSubtitle<'_>) -> Result<()> {
        let display_name = sub.english_name.unwrap_or(sub.entry_name);
        let size_mb = sub.file_size as f64 / 1024.0 / 1024.0;
        let size_str = if size_mb < 0.01 {
            format!("{} B", sub.file_size)
        } else {
            format!("{:.2} MB", size_mb)
        };

        let download_status = if sub.downloaded {
            "✅ 已自动下载"
        } else {
            "⬇️ 点击链接下载"
        };

        let japanese_line = sub
            .japanese_name
            .filter(|n| !n.is_empty() && *n != display_name)
            .map(|n| format!("🇯🇵 <code>{}</code>\n", n))
            .unwrap_or_default();

        let text = format!(
            "🎬 <b>新字幕发布</b>\n\n\
            📺 <b>{}</b>\n\
            {}\
            📝 <code>{}</code>\n\
            📦 大小: <code>{}</code>\n\
            🔗 <a href=\"{}\">下载字幕</a> · <a href=\"https://jimaku.cc/entry/{}\">作品页面</a>\n\n\
            {}",
            display_name,
            japanese_line,
            sub.file_name,
            size_str,
            sub.file_url,
            sub.entry_id,
            download_status
        );

        self.bot
            .send_message(self.chat_id, &text)
            .parse_mode(ParseMode::Html)
            .await?;
        info!("Telegram notification sent for {}", sub.file_name);
        Ok(())
    }

    pub async fn send_message(&self, text: impl Into<String>) -> Result<()> {
        self.bot
            .send_message(self.chat_id, text.into())
            .parse_mode(ParseMode::Html)
            .await?;
        Ok(())
    }

    pub fn chat_id(&self) -> ChatId {
        self.chat_id
    }
}
