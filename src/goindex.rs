use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::{header, StatusCode};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use tracing::debug;

/// GoIndex 源在数据库（`channel_files` / `notified_files`）中使用的固定哨兵 `entry_id`
/// （Jimaku 的真实 `entry_id` 恒为正整数）。
pub const GOINDEX_ENTRY_ID: i64 = -1;

const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const MAX_SCAN_DEPTH: usize = 3;
const MAX_PAGES_PER_DIR: usize = 20;

const SUBTITLE_EXTENSIONS: &[&str] = &[
    "ass", "ssa", "srt", "vtt", "sup", "sub", "idx", "mks", "ttml", "dfxp", "smi", "lrc", "zip",
    "7z", "rar",
];

#[derive(Debug, Clone)]
pub struct GoIndexFile {
    pub name: String,
    pub url: String,
    pub size: i64,
    pub modified_time: DateTime<Utc>,
    /// 从放送文件名提炼出的节目主标题（用于本地目录归档、订阅卡片标题与频道 hashtag）
    pub program_title: String,
}

#[derive(Clone)]
pub struct GoIndexClient {
    client: reqwest::Client,
    base_url: String,
    password: String,
}

#[derive(Debug, Serialize)]
struct ListRequest<'a> {
    password: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_token: Option<&'a str>,
    page_index: usize,
}

#[derive(Debug, Deserialize)]
struct ListResponse {
    #[serde(default)]
    files: Option<Vec<RawItem>>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
    #[serde(default)]
    data: Option<NestedData>,
}

#[derive(Debug, Deserialize)]
struct NestedData {
    #[serde(default)]
    files: Vec<RawItem>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawItem {
    name: String,
    #[serde(default, rename = "mimeType")]
    mime_type: String,
    #[serde(default, deserialize_with = "deserialize_opt_i64")]
    size: Option<i64>,
    #[serde(default, rename = "modifiedTime")]
    modified_time: Option<DateTime<Utc>>,
}

fn deserialize_opt_i64<'de, D>(deserializer: D) -> std::result::Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    let val = Option::<Value>::deserialize(deserializer)?;
    Ok(match val {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_u64().map(|u| u as i64)),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    })
}

impl GoIndexClient {
    pub fn new(base_url: impl Into<String>, password: impl Into<String>) -> Result<Self> {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            header::HeaderValue::from_static("jimaku-subscriber/0.1"),
        );

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .context("Failed to build GoIndex HTTP client")?;

        let base_url = base_url.into().trim().trim_end_matches('/').to_string();
        anyhow::ensure!(
            base_url.starts_with("http://") || base_url.starts_with("https://"),
            "GoIndex base_url must start with http:// or https://"
        );

        Ok(Self {
            client,
            base_url,
            password: password.into(),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 扫描 GoIndex 站点并返回按修改时间升序排列的字幕文件列表
    pub async fn list_subtitle_files(&self) -> Result<Vec<GoIndexFile>> {
        let mut out = Vec::new();
        let mut stack: Vec<(Vec<String>, usize)> = vec![(Vec::new(), 0)];

        while let Some((segments, depth)) = stack.pop() {
            let items = self.list_dir(&segments).await?;
            for item in items {
                if item.mime_type == FOLDER_MIME {
                    if depth < MAX_SCAN_DEPTH && !item.name.starts_with('.') {
                        let mut child = segments.clone();
                        child.push(item.name);
                        stack.push((child, depth + 1));
                    }
                    continue;
                }

                if !is_subtitle_filename(&item.name) {
                    debug!("Skipping non-subtitle GoIndex item: {}", item.name);
                    continue;
                }

                let url = build_file_url(&self.base_url, &segments, &item.name);
                let program_title = extract_program_title(&item.name);
                out.push(GoIndexFile {
                    name: item.name,
                    url,
                    size: item.size.unwrap_or(0),
                    modified_time: item.modified_time.unwrap_or_else(Utc::now),
                    program_title,
                });
            }
        }

        out.sort_by(|a, b| {
            a.modified_time
                .cmp(&b.modified_time)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(out)
    }

    async fn list_dir(&self, segments: &[String]) -> Result<Vec<RawItem>> {
        let dir_url = build_dir_url(&self.base_url, segments);
        let mut all = Vec::new();
        let mut page_token: Option<String> = None;

        for page_index in 0..MAX_PAGES_PER_DIR {
            let req_body = ListRequest {
                password: &self.password,
                page_token: page_token.as_deref(),
                page_index,
            };
            debug!("GoIndex POST {} (page {})", dir_url, page_index);

            let resp = self
                .client
                .post(&dir_url)
                .json(&req_body)
                .send()
                .await
                .with_context(|| format!("Failed to request GoIndex directory {}", dir_url))?;

            let status = resp.status();
            let body = resp
                .text()
                .await
                .context("Failed to read GoIndex response body")?;

            if status != StatusCode::OK {
                anyhow::bail!("GoIndex error ({}): {}", status, truncate_err(&body));
            }

            let parsed: ListResponse = serde_json::from_str(&body).with_context(|| {
                format!("Failed to parse GoIndex JSON: {}", truncate_err(&body))
            })?;

            let (files, next) = if let Some(data) = parsed.data {
                (data.files, data.next_page_token)
            } else {
                (parsed.files.unwrap_or_default(), parsed.next_page_token)
            };

            all.extend(files);
            match next.filter(|t| !t.trim().is_empty()) {
                Some(tok) => page_token = Some(tok),
                None => break,
            }
        }

        Ok(all)
    }
}

fn truncate_err(body: &str) -> String {
    let trimmed = body.trim();
    match trimmed.char_indices().nth(200) {
        Some((idx, _)) => format!("{}...", &trimmed[..idx]),
        None => trimmed.to_string(),
    }
}

pub fn is_subtitle_filename(name: &str) -> bool {
    let Some((_, ext)) = name.rsplit_once('.') else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    SUBTITLE_EXTENSIONS.contains(&ext.as_str())
}

/// 严格按 RFC 3986 unreserved 字符集对单个路径段做百分号编码（确保 `#`、`?`、`[`、`]`、空格、括号等全部编码）
pub fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len() * 2);
    for &b in segment.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                use std::fmt::Write;
                let _ = write!(&mut out, "%{:02X}", b);
            }
        }
    }
    out
}

fn build_dir_url(base_url: &str, segments: &[String]) -> String {
    if segments.is_empty() {
        format!("{}/", base_url)
    } else {
        let encoded = segments
            .iter()
            .map(|s| encode_path_segment(s))
            .collect::<Vec<_>>()
            .join("/");
        format!("{}/{}/", base_url, encoded)
    }
}

fn build_file_url(base_url: &str, dir_segments: &[String], file_name: &str) -> String {
    let encoded_file = encode_path_segment(file_name);
    if dir_segments.is_empty() {
        format!("{}/{}", base_url, encoded_file)
    } else {
        let encoded_dir = dir_segments
            .iter()
            .map(|s| encode_path_segment(s))
            .collect::<Vec<_>>()
            .join("/");
        format!("{}/{}/{}", base_url, encoded_dir, encoded_file)
    }
}

/// 从日本电视放送文件名或常规字幕文件名中提取干净的节目主标题
pub fn extract_program_title(file_name: &str) -> String {
    let stem = strip_subtitle_ext(file_name).trim();
    if stem.is_empty() {
        return file_name.to_string();
    }

    // 1. 剥离放送日期时间 + 电视台前缀：`YYYYMMDD [HHMM] [放送局] `
    let mut s = strip_epg_prefix(stem).unwrap_or_else(|| strip_leading_bracket_group(stem));

    // 2. 移除 EPG 制式属性短标签（如 `[字]`、`[解]`、`[再]`、`[新]`、`[字／日英]` 等）
    let cleaned = remove_epg_flags(s);
    s = cleaned.trim();

    // 3. 剥离前导栏目框（如 `【連続テレビ小説】`、`【ドラマ10】`、`【夜ドラ】`），前提是后面仍有正文
    while let Some(rest) = s.strip_prefix('【') {
        if let Some((_, after)) = rest.split_once('】') {
            let after = after.trim();
            if !after.is_empty() {
                s = after;
                continue;
            }
        }
        break;
    }

    // 4. 在最早出现的集数 / 副标题分隔符处截断
    if let Some(cut) = find_episode_or_subtitle_cut(s) {
        let candidate = s[..cut].trim();
        if !candidate.is_empty() {
            s = candidate;
        }
    }

    // 5. 若末尾附带 `「副標題」` 且前面已有非纯档期主标题，剥离副标题
    if let Some(stripped) = strip_trailing_jp_quote_subtitle(s) {
        s = stripped;
    }

    let final_title = collapse_spaces(s);
    if final_title.is_empty() {
        stem.to_string()
    } else {
        final_title
    }
}

fn strip_subtitle_ext(name: &str) -> &str {
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return name;
    };
    if !SUBTITLE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) {
        return name;
    }
    // 处理双后缀如 `.ja.ass` / `.jpn.srt`
    if let Some((inner, lang)) = stem.rsplit_once('.') {
        let lower = lang.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "ja" | "jp" | "jpn" | "zh" | "chs" | "cht" | "sc" | "tc" | "en" | "eng"
        ) {
            return inner;
        }
    }
    stem
}

fn strip_epg_prefix(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    if bytes.len() < 9 || !bytes[..8].iter().all(u8::is_ascii_digit) || bytes[8] != b' ' {
        return None;
    }
    let mut rest = s[9..].trim_start();
    // 可选 4 位放送时间 HHMM
    if rest.len() >= 5
        && rest.as_bytes()[..4].iter().all(u8::is_ascii_digit)
        && rest.as_bytes()[4] == b' '
    {
        rest = rest[5..].trim_start();
    }
    // 紧随其后的 `[放送局]`
    if let Some(after_open) = rest.strip_prefix('[') {
        if let Some((_, after_station)) = after_open.split_once(']') {
            let trimmed = after_station.trim_start();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }
    Some(rest)
}

fn strip_leading_bracket_group(s: &str) -> &str {
    if let Some(after_open) = s.strip_prefix('[') {
        if let Some((tag, after)) = after_open.split_once(']') {
            let after = after.trim_start();
            if !tag.is_empty() && !after.is_empty() {
                return after;
            }
        }
    }
    s
}

fn remove_epg_flags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open_idx) = rest.find('[') {
        out.push_str(&rest[..open_idx]);
        let after_open = &rest[open_idx + 1..];
        if let Some(close_idx) = after_open.find(']') {
            let inside = &after_open[..close_idx];
            if is_epg_flag(inside) {
                rest = &after_open[close_idx + 1..];
                continue;
            }
            out.push('[');
            out.push_str(inside);
            out.push(']');
            rest = &after_open[close_idx + 1..];
        } else {
            out.push_str(&rest[open_idx..]);
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
}

fn is_epg_flag(tag: &str) -> bool {
    const EXACT_FLAGS: &[&str] = &[
        "字", "解", "再", "新", "終", "二", "多", "デ", "手", "映", "無", "無料", "Ｓ", "S", "SS",
        "Ｂ", "B", "Ｎ", "N", "生", "初", "前", "後", "吹", "双", "天", "交", "CC", "cc",
    ];
    let trimmed = tag.trim();
    if EXACT_FLAGS.contains(&trimmed) {
        return true;
    }
    // 复合标记如 `字／日英`、`二・字`、`字/解`
    if trimmed.chars().count() <= 8
        && (trimmed.starts_with("字／")
            || trimmed.starts_with("字/")
            || trimmed.starts_with("二／")
            || trimmed.starts_with("二・"))
    {
        return true;
    }
    false
}

fn find_episode_or_subtitle_cut(s: &str) -> Option<usize> {
    let mut best: Option<usize> = None;
    let mut update = |pos: usize| {
        if pos > 0 && !s[..pos].trim().is_empty() {
            best = Some(best.map_or(pos, |cur| cur.min(pos)));
        }
    };

    for (idx, ch) in s.char_indices() {
        let tail = &s[idx..];
        match ch {
            // `#05` / ` #54`
            '#' => {
                let after = &tail[1..];
                if after.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                    update(idx);
                }
            }
            // `(27)` / `(1)`（集数圆括号）
            '(' | '（' => {
                let close = if ch == '(' { ')' } else { '）' };
                let after = &tail[ch.len_utf8()..];
                if let Some(end) = after.find(close) {
                    let inner = &after[..end];
                    if !inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit()) {
                        update(idx);
                    }
                }
            }
            // ` 第5話` / ` 第五話` / ` 第12回`
            ' ' | '　' => {
                let after_space = tail[ch.len_utf8()..].trim_start();
                if let Some(after_dai) = after_space.strip_prefix('第') {
                    if starts_with_episode_number(after_dai) {
                        update(idx);
                    }
                }
                if after_space.starts_with('▽')
                    || after_space.starts_with('▼')
                    || after_space.starts_with('~')
                    || after_space.starts_with('～')
                {
                    update(idx);
                }
            }
            _ => {}
        }
    }

    best
}

fn starts_with_episode_number(s: &str) -> bool {
    let is_ep_num = |c: char| {
        c.is_ascii_digit()
            || matches!(
                c,
                '零' | '〇'
                    | '一'
                    | '二'
                    | '三'
                    | '四'
                    | '五'
                    | '六'
                    | '七'
                    | '八'
                    | '九'
                    | '十'
                    | '百'
                    | '０'..='９'
            )
    };
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !is_ep_num(first) {
        return false;
    }
    for c in chars {
        if is_ep_num(c) {
            continue;
        }
        return matches!(c, '話' | '回' | '集' | '弾' | '幕' | '章' | '夜');
    }
    false
}

fn strip_trailing_jp_quote_subtitle(s: &str) -> Option<&str> {
    let trimmed = s.trim_end();
    if !trimmed.ends_with('」') {
        return None;
    }
    let open = trimmed.rfind('「')?;
    let prefix = trimmed[..open].trim();
    // 排除 `連続テレビ小説「ブラッサム」` 这类作品名本身在引号内的情况
    if prefix.is_empty()
        || prefix.ends_with("小説")
        || prefix.ends_with("ドラマ")
        || prefix.ends_with("アニメ")
    {
        return None;
    }
    Some(prefix)
}

fn collapse_spaces(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::{encode_path_segment, extract_program_title, GoIndexClient};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn extracts_clean_titles_from_broadcast_filenames() {
        assert_eq!(
            extract_program_title(
                "20261103 2030 [AT-X] 塩対応の佐藤さんが俺にだけ甘い #05 [字].ass"
            ),
            "塩対応の佐藤さんが俺にだけ甘い"
        );
        assert_eq!(
            extract_program_title("20261103 2300 [AT-X] 千歳くんはラムネ瓶のなか #17 [字].srt"),
            "千歳くんはラムネ瓶のなか"
        );
        assert_eq!(
            extract_program_title(
                "20261103 0800 [NHK総合1・名古屋] 【連続テレビ小説】ブラッサム(27)第6週「ふたり、北へ」[解][字].ass"
            ),
            "ブラッサム"
        );
        assert_eq!(
            extract_program_title(
                "20261103 0115 [フジテレビ] ≠ME 名無しのシンデレラ 第五話 努力の方向性.ass"
            ),
            "≠ME 名無しのシンデレラ"
        );
        assert_eq!(
            extract_program_title(
                "20261103 0430 [東海テレビ011] めざまし昼ドラ・花嫁のれん #54[字][解][再].ass"
            ),
            "めざまし昼ドラ・花嫁のれん"
        );
        assert_eq!(
            extract_program_title(
                "20261104 1800 [NHK BS] ウルトラマン 4Kリマスター版 [新](1)ウルトラ作戦第一号[字].ass"
            ),
            "ウルトラマン 4Kリマスター版"
        );
        assert_eq!(
            extract_program_title(
                "20261104 0104 [中京テレビ1] 太田上田 ~浜辺美波が3年ぶりにやってきた~.srt"
            ),
            "太田上田"
        );
        assert_eq!(
            extract_program_title(
                "20261103 1600 [NHK BS] 岩合光昭の世界ネコ歩き「茨城」[字][再].ass"
            ),
            "岩合光昭の世界ネコ歩き"
        );
    }

    #[test]
    fn encodes_hash_question_brackets_and_spaces() {
        let encoded = encode_path_segment("show #54[字]?.ass");
        assert_eq!(encoded, "show%20%2354%5B%E5%AD%97%5D%3F.ass");
        assert!(!encoded.contains('#'));
        assert!(!encoded.contains('?'));
    }

    #[tokio::test]
    async fn lists_paginated_and_nested_goindex_files() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            for _ in 0..3 {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0u8; 2048];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let body = if req.starts_with("POST / ") && !req.contains("tok2") {
                    r#"{"files":[{"name":"20261103 2030 [AT-X] テスト番組 #02 [字].srt","mimeType":"application/x-subrip","size":"1200","modifiedTime":"2026-11-18T02:00:00Z"},{"name":"sub","mimeType":"application/vnd.google-apps.folder"},{"name":"readme.txt","mimeType":"text/plain","size":"10"}],"nextPageToken":"tok2"}"#
                } else if req.starts_with("POST / ") {
                    r#"{"files":[{"name":"20261103 2000 [AT-X] テスト番組 #01 [字].ass","mimeType":"text/x-ssa","size":3400,"modifiedTime":"2026-11-18T01:00:00Z"}]}"#
                } else {
                    r#"{"data":{"files":[{"name":"nested #3.ass","mimeType":"text/x-ssa","size":"500","modifiedTime":"2026-11-18T03:00:00Z"}]}}"#
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            }
        });

        let client = GoIndexClient {
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            base_url: format!("http://{}", addr),
            password: String::new(),
        };

        let files = client.list_subtitle_files().await.unwrap();
        assert_eq!(files.len(), 3);
        // 按 modifiedTime 升序排列
        assert_eq!(files[0].size, 3400);
        assert_eq!(files[0].program_title, "テスト番組");
        assert_eq!(files[1].size, 1200);
        assert!(files[2].url.ends_with("/sub/nested%20%233.ass"));
    }
}
