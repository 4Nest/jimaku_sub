use anyhow::{Context, Result};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{debug, warn};

const ANILIST_API_URL: &str = "https://graphql.anilist.co";
/// AniList 限速 90 req/min，留余量取 700ms 最小间隔
const MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(700);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// AniList GraphQL 客户端：按 anilist_id 查询作品罗马音标题。
/// 所有失败都降级为 None，绝不阻塞调用方的推送流程。
pub struct AnilistClient {
    client: reqwest::Client,
    last_request: Mutex<Option<Instant>>,
}

impl AnilistClient {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            last_request: Mutex::new(None),
        }
    }

    /// 查询罗马音标题。None 表示查不到或请求失败（调用方自行降级）。
    pub async fn get_romaji(&self, anilist_id: i32) -> Option<String> {
        match self.query_romaji(anilist_id).await {
            Ok(romaji) => romaji,
            Err(e) => {
                warn!("AniList query failed for {}: {:#}", anilist_id, e);
                None
            }
        }
    }

    async fn query_romaji(&self, anilist_id: i32) -> Result<Option<String>> {
        let body = serde_json::json!({
            "query": "query($id: Int) { Media(id: $id, type: ANIME) { title { romaji } } }",
            "variables": { "id": anilist_id },
        });

        // 首次请求；遇到 429 按 Retry-After 重试一次
        for attempt in 0..2 {
            self.throttle().await;
            let resp = self
                .client
                .post(ANILIST_API_URL)
                .json(&body)
                .send()
                .await
                .context("AniList request failed")?;

            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt == 0 {
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(2);
                debug!("AniList 429, retry after {}s", retry_after);
                tokio::time::sleep(Duration::from_secs(retry_after.min(30))).await;
                continue;
            }

            let text = resp.error_for_status()?.text().await?;
            return parse_romaji_response(&text)
                .with_context(|| format!("Failed to parse AniList response: {}", text));
        }
        unreachable!("retry loop returns on second attempt")
    }

    /// 保证两次请求之间至少间隔 MIN_REQUEST_INTERVAL
    async fn throttle(&self) {
        let mut last = self.last_request.lock().await;
        if let Some(prev) = *last {
            let elapsed = prev.elapsed();
            if elapsed < MIN_REQUEST_INTERVAL {
                tokio::time::sleep(MIN_REQUEST_INTERVAL - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }
}

/// 解析 AniList GraphQL 响应。外层 Option=解析是否成功，内层 Option=是否有该条目。
pub(crate) fn parse_romaji_response(text: &str) -> Result<Option<String>> {
    let json: serde_json::Value = serde_json::from_str(text)?;
    let media = &json["data"]["Media"];
    if media.is_null() {
        return Ok(None);
    }
    Ok(media["title"]["romaji"].as_str().map(ToOwned::to_owned))
}

#[cfg(test)]
mod tests {
    use super::parse_romaji_response;

    #[test]
    fn parses_romaji() {
        let json = r#"{"data":{"Media":{"title":{"romaji":"Honzuki no Gekokujou"}}}}"#;
        assert_eq!(
            parse_romaji_response(json).unwrap(),
            Some("Honzuki no Gekokujou".to_string())
        );
    }

    #[test]
    fn parses_null_media_as_none() {
        let json = r#"{"data":{"Media":null}}"#;
        assert_eq!(parse_romaji_response(json).unwrap(), None);
    }

    #[test]
    fn parses_missing_fields_as_none() {
        let json = r#"{"data":{"Media":{"title":{}}}}"#;
        assert_eq!(parse_romaji_response(json).unwrap(), None);
    }

    #[test]
    fn rejects_invalid_json() {
        assert!(parse_romaji_response("not json").is_err());
    }
}
