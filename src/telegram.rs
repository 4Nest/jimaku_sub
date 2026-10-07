use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tracing::{error, info};

#[derive(Clone)]
pub struct TelegramNotifier {
    bot: Bot,
    chat_id: ChatId,
}

impl TelegramNotifier {
    pub fn new(bot_token: impl Into<String>, chat_id: impl Into<String>) -> Result<Self> {
        let bot = Bot::new(bot_token.into());
        let chat_id_str = chat_id.into();
        let chat_id = if let Ok(id) = chat_id_str.parse::<i64>() {
            ChatId(id)
        } else {
            // 支持 @channelusername 格式
            ChatId(0)
        };
        Ok(Self { bot, chat_id })
    }

    #[allow(dead_code)]
    pub fn new_with_parsed(bot: Bot, chat_id: ChatId) -> Self {
        Self { bot, chat_id }
    }

    pub async fn notify_new_subtitle(
        &self,
        entry_name: &str,
        english_name: Option<&str>,
        japanese_name: Option<&str>,
        file_name: &str,
        file_size: i64,
        file_url: &str,
        _entry_id: i64,
        downloaded: bool,
    ) -> Result<()> {
        let display_name = english_name.unwrap_or(entry_name);
        let size_mb = file_size as f64 / 1024.0 / 1024.0;
        let size_str = if size_mb < 0.01 {
            format!("{} B", file_size)
        } else {
            format!("{:.2} MB", size_mb)
        };

        let download_status = if downloaded {
            "✅ 已自动下载"
        } else {
            "⬇️ 点击链接下载"
        };

        let japanese_line = japanese_name
            .filter(|n| !n.is_empty() && *n != display_name)
            .map(|n| format!("🇯🇵 <code>{}</code>\n", n))
            .unwrap_or_default();

        let text = format!(
            "🎬 <b>新字幕发布</b>\n\n\
            📺 <b>{}</b>\n\
            {}\
            📝 <code>{}</code>\n\
            📦 大小: <code>{}</code>\n\
            🔗 <a href=\"{}\">下载字幕</a>\n\n\
            {}",
            display_name, japanese_line, file_name, size_str, file_url, download_status
        );

        match self
            .bot
            .send_message(self.chat_id, &text)
            .parse_mode(ParseMode::Html)
            .await
        {
            Ok(_) => info!("Telegram notification sent for {}", file_name),
            Err(e) => error!("Failed to send Telegram notification: {}", e),
        }

        Ok(())
    }

    pub async fn send_message(&self, text: impl Into<String>) -> Result<()> {
        self.bot
            .send_message(self.chat_id, text.into())
            .parse_mode(ParseMode::Html)
            .await?;
        Ok(())
    }

    pub fn bot(&self) -> &Bot {
        &self.bot
    }

    #[allow(dead_code)]
    pub fn chat_id(&self) -> ChatId {
        self.chat_id
    }
}
