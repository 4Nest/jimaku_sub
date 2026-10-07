use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(default = "default_jimaku")]
    pub jimaku: JimakuConfig,
    #[serde(default = "default_telegram")]
    pub telegram: TelegramConfig,
    #[serde(default = "default_subscription")]
    pub subscription: SubscriptionConfig,
    #[serde(default = "default_download")]
    pub download: DownloadConfig,
    #[serde(default = "default_scheduler")]
    pub scheduler: SchedulerConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JimakuConfig {
    pub api_key: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub chat_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SubscriptionConfig {
    #[serde(default)]
    pub anilist_ids: Vec<i32>,
    #[serde(default)]
    pub name_keywords: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DownloadConfig {
    #[serde(default = "default_false")]
    pub enabled: bool,
    #[serde(default = "default_download_path")]
    pub download_path: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchedulerConfig {
    #[serde(default = "default_interval_seconds")]
    pub interval_seconds: u64,
}

fn default_jimaku() -> JimakuConfig {
    JimakuConfig {
        api_key: String::new(),
        base_url: default_base_url(),
    }
}

fn default_telegram() -> TelegramConfig {
    TelegramConfig {
        bot_token: String::new(),
        chat_id: String::new(),
    }
}

fn default_subscription() -> SubscriptionConfig {
    SubscriptionConfig::default()
}

fn default_download() -> DownloadConfig {
    DownloadConfig {
        enabled: false,
        download_path: default_download_path(),
    }
}

fn default_scheduler() -> SchedulerConfig {
    SchedulerConfig {
        interval_seconds: default_interval_seconds(),
    }
}

fn default_base_url() -> String {
    "https://jimaku.cc".to_string()
}

fn default_download_path() -> String {
    "/app/downloads".to_string()
}

fn default_interval_seconds() -> u64 {
    300
}

fn default_false() -> bool {
    false
}

impl Config {
    pub fn load() -> Result<Self> {
        let mut builder = config::Config::builder();

        // 默认配置
        builder = builder.set_default("jimaku.api_key", "")?;
        builder = builder.set_default("jimaku.base_url", "https://jimaku.cc")?;
        builder = builder.set_default("telegram.bot_token", "")?;
        builder = builder.set_default("telegram.chat_id", "")?;
        builder = builder.set_default("download.enabled", false)?;
        builder = builder.set_default("download.download_path", "/app/downloads")?;
        builder = builder.set_default("scheduler.interval_seconds", 300)?;

        // 从配置文件读取
        if Path::new("config.toml").exists() {
            builder = builder.add_source(config::File::with_name("config.toml"));
        }

        // 从环境变量读取
        builder = builder.add_source(
            config::Environment::with_prefix("")
                .separator("__")
                .try_parsing(true),
        );

        let mut cfg: Config = builder.build()?.try_deserialize()?;

        // 特殊处理环境变量（支持 JIMAKU_API_KEY, TELEGRAM_BOT_TOKEN 等）
        if let Ok(v) = std::env::var("JIMAKU_API_KEY") {
            cfg.jimaku.api_key = v;
        }
        if let Ok(v) = std::env::var("TELEGRAM_BOT_TOKEN") {
            cfg.telegram.bot_token = v;
        }
        if let Ok(v) = std::env::var("TELEGRAM_CHAT_ID") {
            cfg.telegram.chat_id = v;
        }
        if let Ok(v) = std::env::var("DOWNLOAD_ENABLED") {
            cfg.download.enabled = v.parse().unwrap_or(false);
        }
        if let Ok(v) = std::env::var("DOWNLOAD_PATH") {
            cfg.download.download_path = v;
        }
        if let Ok(v) = std::env::var("SCHEDULER_INTERVAL_SECONDS") {
            cfg.scheduler.interval_seconds = v.parse().unwrap_or(300);
        }
        // 解析逗号分隔的 anilist_ids
        if let Ok(v) = std::env::var("SUBSCRIPTION_ANILIST_IDS") {
            cfg.subscription.anilist_ids = v
                .split(',')
                .filter_map(|s| s.trim().parse::<i32>().ok())
                .collect();
        }
        // 解析逗号分隔的 name_keywords
        if let Ok(v) = std::env::var("SUBSCRIPTION_NAME_KEYWORDS") {
            cfg.subscription.name_keywords = v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .collect();
        }

        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.jimaku.api_key.is_empty() {
            anyhow::bail!("Jimaku API key is required");
        }
        if self.telegram.bot_token.is_empty() {
            anyhow::bail!("Telegram bot token is required");
        }
        if self.telegram.chat_id.is_empty() {
            anyhow::bail!("Telegram chat ID is required");
        }
        Ok(())
    }
}
