use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use teloxide::adaptors::throttle::Limits;
use teloxide::adaptors::Throttle;
use teloxide::prelude::*;
use teloxide::types::{InputFile, ParseMode};
use tracing::info;

/// 通知用 Bot：带限流，自动遵循 Telegram API 速率限制并在 429 时等待重试
pub type ThrottledBot = Throttle<Bot>;

/// 一条新字幕通知所需的全部信息
pub struct NewSubtitle<'a> {
    /// notified_files 表行 id，用于下载按钮 callback
    pub file_id: i64,
    pub entry_name: &'a str,
    pub english_name: Option<&'a str>,
    pub japanese_name: Option<&'a str>,
    pub file_name: &'a str,
    pub file_size: i64,
    pub file_url: &'a str,
    pub file_modified: DateTime<Utc>,
    pub entry_id: i64,
    pub downloaded: bool,
}

/// Telegram 文档来源：本地已落盘路径，或不落盘的内存字节流
pub enum DocumentSource<'a> {
    Path(&'a std::path::Path),
    Memory { file_name: &'a str, data: Vec<u8> },
}

impl DocumentSource<'_> {
    pub fn into_input_file(self) -> InputFile {
        match self {
            Self::Path(path) => InputFile::file(path),
            Self::Memory { file_name, data } => {
                InputFile::memory(data).file_name(file_name.to_string())
            }
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Path(path) => format!("{:?}", path),
            Self::Memory { file_name, data } => {
                format!("{} ({} bytes, memory)", file_name, data.len())
            }
        }
    }
}

#[derive(Clone)]
pub struct TelegramNotifier {
    bot: ThrottledBot,
    chat_id: ChatId,
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
        let text = crate::bot::text::new_subtitle_card(sub);
        let keyboard = crate::bot::keyboards::download_keyboard(sub.file_id);

        self.bot
            .send_message(self.chat_id, text)
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboard)
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

    /// 发送字幕文档到私聊（支持本地文件或内存字节流）
    pub async fn send_document(&self, source: DocumentSource<'_>) -> Result<()> {
        let label = source.label();
        self.bot
            .send_document(self.chat_id, source.into_input_file())
            .await?;
        info!("Telegram document sent: {}", label);
        Ok(())
    }

    pub fn chat_id(&self) -> ChatId {
        self.chat_id
    }

    /// 共享限流 Bot 实例（ChannelNotifier 复用同一限流队列，避免叠加触发 429）
    pub fn bot(&self) -> ThrottledBot {
        self.bot.clone()
    }
}

/// 全量字幕频道推送：与私聊通知共享限流 Bot，目标为频道
#[derive(Clone)]
pub struct ChannelNotifier {
    bot: ThrottledBot,
    chat_id: ChatId,
}

impl ChannelNotifier {
    pub fn new(bot: ThrottledBot, chat_id: impl AsRef<str>) -> Result<Self> {
        let raw = chat_id.as_ref().trim();
        let id: i64 = raw
            .parse()
            .context("CHANNEL_CHAT_ID 必须是数字频道 id（-100 开头，不支持 @username）")?;
        anyhow::ensure!(
            id < 0,
            "CHANNEL_CHAT_ID 必须是频道/群组 id（负数，-100 开头），当前值: {}",
            raw
        );
        Ok(Self {
            bot,
            chat_id: ChatId(id),
        })
    }

    /// 启动时静默验证频道可访问（bot 已加管理员），不发任何消息
    pub async fn verify_access(&self) -> Result<()> {
        self.bot.get_chat(self.chat_id).await?;
        Ok(())
    }

    /// 发送字幕文件到频道，Caption 为 MarkdownV2（≤1024 字符）
    pub async fn send_subtitle_document(
        &self,
        source: DocumentSource<'_>,
        caption: &str,
    ) -> Result<()> {
        let label = source.label();
        self.bot
            .send_document(self.chat_id, source.into_input_file())
            .caption(caption)
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
        info!("Channel document sent: {}", label);
        Ok(())
    }

    /// 发送文字卡片到频道（MarkdownV2，超大文件的链接卡片等）
    pub async fn send_text_card(&self, text: &str) -> Result<()> {
        self.bot
            .send_message(self.chat_id, text)
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
        Ok(())
    }
}
