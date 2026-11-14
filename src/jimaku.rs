use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::{
    header::{self, HeaderMap, HeaderValue},
    StatusCode, Url,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use tracing::debug;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Entry {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub english_name: Option<String>,
    #[serde(default)]
    pub japanese_name: Option<String>,
    #[serde(default)]
    pub anilist_id: Option<i32>,
    #[serde(default)]
    pub tmdb_id: Option<String>,
    pub last_modified: DateTime<Utc>,
    #[serde(default)]
    pub flags: Value,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub creator_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FileEntry {
    pub name: String,
    pub url: String,
    pub size: i64,
    pub last_modified: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ApiError {
    message: String,
}

#[derive(Clone)]
pub struct JimakuClient {
    client: reqwest::Client,
    base_url: String,
}

impl JimakuClient {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Result<Self> {
        let mut headers = HeaderMap::new();
        let api_key = normalize_api_key(api_key.into());
        let auth = HeaderValue::from_str(&api_key).context("Invalid API key format")?;
        headers.insert(header::AUTHORIZATION, auth);
        headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .context("Failed to build HTTP client")?;

        let base_url = base_url.into().trim().trim_end_matches('/').to_string();

        Ok(Self { client, base_url })
    }

    pub async fn search_entries(
        &self,
        after: Option<i64>,
        before: Option<i64>,
    ) -> Result<Vec<Entry>> {
        let endpoint = format!("{}/api/entries/search", self.base_url);
        let mut params = Vec::with_capacity(2);
        if let Some(ts) = after {
            params.push(("after", ts.to_string()));
        }
        if let Some(ts) = before {
            params.push(("before", ts.to_string()));
        }
        let url = Url::parse_with_params(&endpoint, &params)
            .context("Failed to build Jimaku search URL")?;
        debug!("Jimaku search URL: {}", url);
        self.get_json(url, "entries search").await
    }

    pub async fn search_entries_by_query(&self, query: &str) -> Result<Vec<Entry>> {
        let url = Url::parse_with_params(
            &format!("{}/api/entries/search", self.base_url),
            &[("query", query)],
        )
        .context("Failed to build Jimaku query URL")?;
        debug!("Jimaku query URL: {}", url);
        self.get_json(url, "entries query").await
    }

    pub async fn get_entry_files(&self, entry_id: i64) -> Result<Vec<FileEntry>> {
        let url = Url::parse(&format!("{}/api/entries/{}/files", self.base_url, entry_id))
            .context("Failed to build Jimaku entry files URL")?;
        match self.get_json_opt(url, "entry files").await? {
            Some(files) => Ok(files),
            None => Ok(Vec::new()),
        }
    }

    pub async fn get_entry_by_id(&self, entry_id: i64) -> Result<Entry> {
        let url = Url::parse(&format!("{}/api/entries/{}", self.base_url, entry_id))
            .context("Failed to build Jimaku entry URL")?;
        self.get_json(url, "entry details").await
    }

    async fn get_json<T: DeserializeOwned>(&self, url: Url, label: &str) -> Result<T> {
        let (status, body) = self.fetch_body(url, label).await?;
        if !status.is_success() {
            return Err(api_error(status, &body));
        }
        serde_json::from_str(&body).with_context(|| format!("Failed to parse {label} response"))
    }

    async fn get_json_opt<T: DeserializeOwned>(&self, url: Url, label: &str) -> Result<Option<T>> {
        let (status, body) = self.fetch_body(url, label).await?;
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(api_error(status, &body));
        }
        let parsed = serde_json::from_str(&body)
            .with_context(|| format!("Failed to parse {label} response"))?;
        Ok(Some(parsed))
    }

    async fn fetch_body(&self, url: Url, label: &str) -> Result<(StatusCode, String)> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .with_context(|| format!("Failed to send {label} request"))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .with_context(|| format!("Failed to read {label} response body"))?;
        Ok((status, body))
    }

    /// 检查条目是否匹配订阅条件（调用方应确保至少有一项过滤条件非空）
    pub fn matches_subscription(
        entry: &Entry,
        anilist_ids: &[i32],
        name_keywords: &[String],
    ) -> bool {
        if anilist_ids.is_empty() && name_keywords.is_empty() {
            return false;
        }

        if let Some(aid) = entry.anilist_id {
            if anilist_ids.contains(&aid) {
                return true;
            }
        }

        if name_keywords.is_empty() {
            return false;
        }

        let name_lower = entry.name.to_lowercase();
        let en_lower = entry.english_name.as_deref().map(str::to_lowercase);
        let ja_lower = entry.japanese_name.as_deref().map(str::to_lowercase);

        name_keywords
            .iter()
            .map(|kw| kw.trim())
            .filter(|kw| !kw.is_empty())
            .any(|kw| {
                let kw = kw.to_lowercase();
                name_lower.contains(&kw)
                    || en_lower.as_deref().is_some_and(|s| s.contains(&kw))
                    || ja_lower.as_deref().is_some_and(|s| s.contains(&kw))
            })
    }
}

fn api_error(status: StatusCode, body: &str) -> anyhow::Error {
    if let Ok(err) = serde_json::from_str::<ApiError>(body) {
        anyhow::anyhow!("Jimaku API error ({}): {}", status, err.message)
    } else {
        anyhow::anyhow!("Jimaku API error ({}): {}", status, body)
    }
}

fn normalize_api_key(api_key: String) -> String {
    let trimmed = api_key
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .trim();

    trimmed
        .strip_prefix("Bearer ")
        .or_else(|| trimmed.strip_prefix("bearer "))
        .unwrap_or(trimmed)
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{normalize_api_key, Entry, JimakuClient};
    use chrono::Utc;

    #[test]
    fn normalizes_common_api_key_wrappers() {
        assert_eq!(normalize_api_key(" abc123 ".to_string()), "abc123");
        assert_eq!(normalize_api_key("\"abc123\"".to_string()), "abc123");
        assert_eq!(normalize_api_key("Bearer abc123".to_string()), "abc123");
    }

    #[test]
    fn parses_entry_with_object_flags() {
        let raw = r#"{
            "id": 11787,
            "name": "Yomi no Tsugai",
            "flags": {},
            "last_modified": "2026-05-23T22:50:20Z",
            "creator_id": 15,
            "anilist_id": 195600,
            "english_name": "Daemons of the Shadow Realm",
            "japanese_name": "黄泉のツガイ"
        }"#;

        let entry: Entry = serde_json::from_str(raw).unwrap();
        assert_eq!(entry.id, 11787);
        assert_eq!(entry.flags, serde_json::json!({}));
    }

    #[test]
    fn empty_or_blank_subscription_filters_never_match() {
        let entry = Entry {
            id: 1,
            name: "Show".to_string(),
            english_name: None,
            japanese_name: None,
            anilist_id: Some(42),
            tmdb_id: None,
            last_modified: Utc::now(),
            flags: serde_json::json!({}),
            notes: None,
            creator_id: None,
        };
        assert!(!JimakuClient::matches_subscription(&entry, &[], &[]));
        assert!(!JimakuClient::matches_subscription(
            &entry,
            &[],
            &["   ".to_string()]
        ));
        assert!(JimakuClient::matches_subscription(
            &entry,
            &[],
            &["show".to_string()]
        ));
    }
}
