use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub jimaku: JimakuConfig,
    #[serde(default)]
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub subscription: SubscriptionConfig,
    #[serde(default)]
    pub download: DownloadConfig,
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub channel: ChannelConfig,
    #[serde(default)]
    pub goindex: GoIndexConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JimakuConfig {
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
}

impl Default for JimakuConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: default_base_url(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TelegramConfig {
    #[serde(default)]
    pub bot_token: String,
    #[serde(default)]
    pub chat_id: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SubscriptionConfig {
    #[serde(default)]
    pub anilist_ids: Vec<i32>,
    #[serde(default)]
    pub name_keywords: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DownloadConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_download_path")]
    pub download_path: String,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            download_path: default_download_path(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchedulerConfig {
    #[serde(default = "default_interval_seconds")]
    pub interval_seconds: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            interval_seconds: default_interval_seconds(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DatabaseConfig {
    #[serde(default = "default_database_url")]
    pub url: String,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: default_database_url(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_dir")]
    pub dir: String,
    #[serde(default = "default_log_retention_days")]
    pub retention_days: u32,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            dir: default_log_dir(),
            retention_days: default_log_retention_days(),
        }
    }
}

/// 全量字幕频道推送
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ChannelConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 数字频道 id（-100 开头），不支持 @username
    #[serde(default)]
    pub chat_id: String,
}

/// GoIndex 放送字幕源配置（默认启用 subtitle.nanakofansub.workers.dev）
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GoIndexConfig {
    #[serde(default = "default_goindex_enabled")]
    pub enabled: bool,
    #[serde(default = "default_goindex_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub password: String,
}

impl Default for GoIndexConfig {
    fn default() -> Self {
        Self {
            enabled: default_goindex_enabled(),
            base_url: default_goindex_base_url(),
            password: String::new(),
        }
    }
}

fn default_goindex_enabled() -> bool {
    true
}

fn default_goindex_base_url() -> String {
    "https://subtitle.nanakofansub.workers.dev".to_string()
}

fn default_database_url() -> String {
    "sqlite://data/jimaku_subscriber.db".to_string()
}

fn default_log_dir() -> String {
    "./logs".to_string()
}

fn default_log_retention_days() -> u32 {
    30
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

impl Config {
    pub fn load() -> Result<Self> {
        Self::load_from_path_and_env(Path::new("config.toml"), |key| std::env::var(key).ok())
    }

    pub fn load_from_path_and_env(
        path: &Path,
        get_env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let mut cfg = if path.exists() {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read config file: {}", path.display()))?;
            toml::from_str::<Config>(&raw)
                .with_context(|| format!("Failed to parse config file: {}", path.display()))?
        } else {
            Config::default()
        };

        cfg.apply_env_overrides(get_env)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn apply_env_overrides(&mut self, get_env: impl Fn(&str) -> Option<String>) -> Result<()> {
        let non_empty = |key: &str| -> Option<String> {
            get_env(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        if let Some(v) = non_empty("JIMAKU_API_KEY") {
            self.jimaku.api_key = v;
        }
        if let Some(v) = non_empty("JIMAKU_BASE_URL") {
            self.jimaku.base_url = v;
        }
        if let Some(v) = non_empty("TELEGRAM_BOT_TOKEN") {
            self.telegram.bot_token = v;
        }
        if let Some(v) = non_empty("TELEGRAM_CHAT_ID") {
            self.telegram.chat_id = v;
        }
        if let Some(v) = non_empty("DOWNLOAD_ENABLED") {
            self.download.enabled = parse_bool_env("DOWNLOAD_ENABLED", &v)?;
        }
        if let Some(v) = non_empty("DOWNLOAD_PATH") {
            self.download.download_path = v;
        }
        if let Some(v) = non_empty("SCHEDULER_INTERVAL_SECONDS") {
            self.scheduler.interval_seconds = v
                .parse()
                .with_context(|| format!("Invalid SCHEDULER_INTERVAL_SECONDS: {v}"))?;
        }
        if let Some(v) = non_empty("DATABASE_URL") {
            self.database.url = v;
        }
        if let Some(v) = non_empty("LOG_DIR") {
            self.logging.dir = v;
        }
        if let Some(v) = non_empty("LOG_RETENTION_DAYS") {
            self.logging.retention_days = v
                .parse()
                .with_context(|| format!("Invalid LOG_RETENTION_DAYS: {v}"))?;
        }
        if let Some(v) = non_empty("CHANNEL_ENABLED") {
            self.channel.enabled = parse_bool_env("CHANNEL_ENABLED", &v)?;
        }
        if let Some(v) = non_empty("CHANNEL_CHAT_ID") {
            self.channel.chat_id = v;
        }
        if let Some(v) = non_empty("GOINDEX_ENABLED") {
            self.goindex.enabled = parse_bool_env("GOINDEX_ENABLED", &v)?;
        }
        if let Some(v) = non_empty("GOINDEX_BASE_URL") {
            self.goindex.base_url = v;
        }
        if let Some(v) = non_empty("GOINDEX_PASSWORD") {
            self.goindex.password = v;
        }
        if let Some(v) = non_empty("SUBSCRIPTION_ANILIST_IDS") {
            self.subscription.anilist_ids = v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| {
                    s.parse::<i32>().with_context(|| {
                        format!("Invalid AniList ID in SUBSCRIPTION_ANILIST_IDS: {s}")
                    })
                })
                .collect::<Result<Vec<_>>>()?;
        }
        if let Some(v) = non_empty("SUBSCRIPTION_NAME_KEYWORDS") {
            self.subscription.name_keywords = v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .collect();
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.jimaku.api_key.trim().is_empty() {
            anyhow::bail!("Jimaku API key is required");
        }
        if self.telegram.bot_token.trim().is_empty() {
            anyhow::bail!("Telegram bot token is required");
        }
        if self.telegram.chat_id.trim().is_empty() {
            anyhow::bail!("Telegram chat ID is required");
        }
        if self.scheduler.interval_seconds == 0 {
            anyhow::bail!("SCHEDULER_INTERVAL_SECONDS must be greater than 0");
        }
        if self.channel.enabled {
            match self.channel.chat_id.trim().parse::<i64>() {
                Ok(id) if id < 0 => {}
                _ => anyhow::bail!(
                    "CHANNEL_CHAT_ID must be a numeric channel id (-100...) when channel is enabled"
                ),
            }
        }
        if self.goindex.enabled {
            let url = self.goindex.base_url.trim();
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                anyhow::bail!("GOINDEX_BASE_URL must start with http:// or https://");
            }
        }
        Ok(())
    }
}

fn parse_bool_env(name: &str, raw: &str) -> Result<bool> {
    match raw.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => anyhow::bail!("Invalid boolean for {name}: {raw}"),
    }
}

#[cfg(test)]
mod tests {
    use super::Config;
    use std::collections::HashMap;

    #[test]
    fn empty_env_does_not_override_toml_values() {
        let dir = std::env::temp_dir().join(format!("jimaku-cfg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"
            [jimaku]
            api_key = "k"
            [telegram]
            bot_token = "t"
            chat_id = "123"
            [download]
            enabled = true
            [channel]
            enabled = true
            chat_id = "-100123456"
            "#,
        )
        .unwrap();

        let env = HashMap::from([("CHANNEL_CHAT_ID", ""), ("SUBSCRIPTION_ANILIST_IDS", "   ")]);
        let cfg = Config::load_from_path_and_env(&path, |k| env.get(k).map(|v| (*v).to_string()))
            .unwrap();
        assert!(cfg.download.enabled);
        assert!(cfg.channel.enabled);
        assert_eq!(cfg.channel.chat_id, "-100123456");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_zero_scheduler_interval() {
        let env = HashMap::from([
            ("JIMAKU_API_KEY", "k"),
            ("TELEGRAM_BOT_TOKEN", "t"),
            ("TELEGRAM_CHAT_ID", "123"),
            ("SCHEDULER_INTERVAL_SECONDS", "0"),
        ]);
        let missing = std::path::Path::new("/nonexistent-jimaku-config.toml");
        let err = Config::load_from_path_and_env(missing, |k| env.get(k).map(|v| (*v).to_string()))
            .unwrap_err();
        assert!(err.to_string().contains("SCHEDULER_INTERVAL_SECONDS"));
    }
}
