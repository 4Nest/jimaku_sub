use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::header::{self, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
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
pub struct ApiError {
    pub message: String,
}

pub struct JimakuClient {
    client: reqwest::Client,
    base_url: String,
    #[allow(dead_code)]
    api_key: String,
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
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self {
            client,
            base_url: base_url.into(),
            api_key,
        })
    }

    pub async fn search_entries(
        &self,
        after: Option<i64>,
        before: Option<i64>,
    ) -> Result<Vec<Entry>> {
        let mut url = format!("{}/api/entries/search", self.base_url);
        let mut params = vec![];
        if let Some(ts) = after {
            params.push(format!("after={}", ts));
        }
        if let Some(ts) = before {
            params.push(format!("before={}", ts));
        }
        if !params.is_empty() {
            url.push('?');
            url.push_str(&params.join("&"));
        }

        debug!("Jimaku search URL: {}", url);

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to send search request")?;

        let status = resp.status();
        let body = resp.text().await.context("Failed to read response body")?;

        if !status.is_success() {
            if let Ok(err) = serde_json::from_str::<ApiError>(&body) {
                anyhow::bail!("Jimaku API error ({}): {}", status, err.message);
            }
            anyhow::bail!("Jimaku API error ({}): {}", status, body);
        }

        let entries: Vec<Entry> =
            serde_json::from_str(&body).context("Failed to parse entries response")?;
        Ok(entries)
    }

    pub async fn search_entries_by_query(&self, query: &str) -> Result<Vec<Entry>> {
        let url = reqwest::Url::parse_with_params(
            &format!("{}/api/entries/search", self.base_url),
            &[("query", query)],
        )
        .context("Failed to build Jimaku query URL")?;

        debug!("Jimaku query URL: {}", url);

        let resp = self
            .client
            .get(url)
            .send()
            .await
            .context("Failed to send query request")?;

        let status = resp.status();
        let body = resp.text().await.context("Failed to read response body")?;

        if !status.is_success() {
            if let Ok(err) = serde_json::from_str::<ApiError>(&body) {
                anyhow::bail!("Jimaku API error ({}): {}", status, err.message);
            }
            anyhow::bail!("Jimaku API error ({}): {}", status, body);
        }

        let entries: Vec<Entry> =
            serde_json::from_str(&body).context("Failed to parse query response")?;
        Ok(entries)
    }

    pub async fn get_entry_files(&self, entry_id: i64) -> Result<Vec<FileEntry>> {
        let url = format!("{}/api/entries/{}/files", self.base_url, entry_id);

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to send files request")?;

        let status = resp.status();
        let body = resp.text().await.context("Failed to read response body")?;

        if status == 404 {
            return Ok(vec![]);
        }

        if !status.is_success() {
            if let Ok(err) = serde_json::from_str::<ApiError>(&body) {
                anyhow::bail!("Jimaku API error ({}): {}", status, err.message);
            }
            anyhow::bail!("Jimaku API error ({}): {}", status, body);
        }

        let files: Vec<FileEntry> =
            serde_json::from_str(&body).context("Failed to parse files response")?;
        Ok(files)
    }

    #[allow(dead_code)]
    pub async fn get_entry_by_id(&self, entry_id: i64) -> Result<Entry> {
        let url = format!("{}/api/entries/{}", self.base_url, entry_id);

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to send entry details request")?;

        let status = resp.status();
        let body = resp.text().await.context("Failed to read response body")?;

        if !status.is_success() {
            if let Ok(err) = serde_json::from_str::<ApiError>(&body) {
                anyhow::bail!("Jimaku API error ({}): {}", status, err.message);
            }
            anyhow::bail!("Jimaku API error ({}): {}", status, body);
        }

        let entry: Entry = serde_json::from_str(&body).context("Failed to parse entry response")?;
        Ok(entry)
    }

    /// 检查条目是否匹配订阅条件
    pub fn matches_subscription(
        entry: &Entry,
        anilist_ids: &[i32],
        name_keywords: &[String],
    ) -> bool {
        // 无条件 = 全部订阅
        if anilist_ids.is_empty() && name_keywords.is_empty() {
            return true;
        }

        // 检查 anilist_id
        if let Some(aid) = entry.anilist_id {
            if anilist_ids.contains(&aid) {
                return true;
            }
        }

        // 检查关键词（name, english_name, japanese_name）
        let haystack = format!(
            "{} {} {}",
            entry.name,
            entry.english_name.as_deref().unwrap_or(""),
            entry.japanese_name.as_deref().unwrap_or("")
        )
        .to_lowercase();

        for kw in name_keywords {
            if haystack.contains(&kw.to_lowercase()) {
                return true;
            }
        }

        false
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
    use super::{normalize_api_key, Entry};

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
}
