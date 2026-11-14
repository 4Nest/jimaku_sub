use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::{fs, io::AsyncWriteExt};
use tracing::{debug, info};

/// 单个路径段最大 UTF-8 字节数（预留临时文件 `.part.<id>` 后缀，严守 Linux ext4 255 字节上限）
const MAX_COMPONENT_BYTES: usize = 200;

pub struct Downloader {
    client: reqwest::Client,
    download_path: PathBuf,
}

pub struct DownloadOutcome {
    pub path: PathBuf,
    pub size: u64,
    pub skipped_existing: bool,
}

/// 不落盘直取字幕字节流（用于未开启本地自动下载时的 Telegram 发送）
pub enum FetchedSubtitle {
    Bytes(Vec<u8>),
    TooLarge,
}

impl Downloader {
    pub fn new(download_path: impl Into<PathBuf>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            download_path: download_path.into(),
        }
    }

    /// 计算某个字幕文件的本地路径（不保证文件存在）
    pub fn local_path(&self, entry_name: &str, file_name: &str) -> PathBuf {
        self.download_path
            .join(sanitize_filename(entry_name))
            .join(sanitize_filename(file_name))
    }

    /// 流式下载字幕并原子写入目标路径（先写 `.part` 临时文件再 `rename`，杜绝半截脏文件）
    pub async fn download_subtitle_with_outcome(
        &self,
        entry_name: &str,
        file_name: &str,
        file_url: &str,
    ) -> Result<DownloadOutcome> {
        let safe_entry = sanitize_filename(entry_name);
        let safe_file = sanitize_filename(file_name);

        let dir = self.download_path.join(&safe_entry);
        fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("Failed to create directory: {:?}", dir))?;

        let file_path = dir.join(&safe_file);

        if let Ok(metadata) = fs::metadata(&file_path).await {
            if metadata.len() > 0 {
                debug!("File already exists, skipping: {:?}", file_path);
                return Ok(DownloadOutcome {
                    path: file_path,
                    size: metadata.len(),
                    skipped_existing: true,
                });
            }
        }

        info!("Downloading: {} -> {:?}", file_url, file_path);

        let mut resp = self
            .client
            .get(file_url)
            .send()
            .await
            .with_context(|| format!("Failed to download from {}", file_url))?;

        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("Download failed with status: {}", status);
        }

        let tmp_name = format!(
            ".{}.{}.part",
            safe_file,
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        let part_path = dir.join(tmp_name);

        let write_result: Result<u64> = async {
            let mut file = fs::File::create(&part_path)
                .await
                .with_context(|| format!("Failed to create temp file: {:?}", part_path))?;
            let mut total: u64 = 0;
            while let Some(chunk) = resp
                .chunk()
                .await
                .context("Failed to read download chunk")?
            {
                file.write_all(&chunk)
                    .await
                    .with_context(|| format!("Failed to write chunk to {:?}", part_path))?;
                total += chunk.len() as u64;
            }
            file.flush()
                .await
                .with_context(|| format!("Failed to flush temp file: {:?}", part_path))?;
            Ok(total)
        }
        .await;

        let total = match write_result {
            Ok(total) => total,
            Err(e) => {
                let _ = fs::remove_file(&part_path).await;
                return Err(e);
            }
        };

        if let Err(e) = fs::rename(&part_path, &file_path).await {
            let _ = fs::remove_file(&part_path).await;
            return Err(e).with_context(|| format!("Failed to finalize file: {:?}", file_path));
        }

        info!(
            "Downloaded {} ({} bytes) to {:?}",
            safe_file, total, file_path
        );
        Ok(DownloadOutcome {
            path: file_path,
            size: total,
            skipped_existing: false,
        })
    }

    /// 流式读取字幕到内存，一旦超过 `max_bytes` 立即中止并返回 `TooLarge`（零磁盘残留、防 OOM）
    pub async fn fetch_subtitle_bytes(
        &self,
        file_url: &str,
        max_bytes: usize,
    ) -> Result<FetchedSubtitle> {
        let mut resp = self
            .client
            .get(file_url)
            .send()
            .await
            .with_context(|| format!("Failed to download from {}", file_url))?;

        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("Download failed with status: {}", status);
        }

        if let Some(len) = resp.content_length() {
            if len > max_bytes as u64 {
                return Ok(FetchedSubtitle::TooLarge);
            }
        }

        let cap = resp
            .content_length()
            .map(|n| (n as usize).min(max_bytes))
            .unwrap_or(64 * 1024);
        let mut buf = Vec::with_capacity(cap);

        while let Some(chunk) = resp
            .chunk()
            .await
            .context("Failed to read download chunk")?
        {
            if buf.len() + chunk.len() > max_bytes {
                return Ok(FetchedSubtitle::TooLarge);
            }
            buf.extend_from_slice(&chunk);
        }

        Ok(FetchedSubtitle::Bytes(buf))
    }
}

pub fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();

    let trimmed = cleaned.trim().trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return "_".to_string();
    }

    if trimmed.len() <= MAX_COMPONENT_BYTES {
        return trimmed.to_string();
    }

    // 超长文件名：保留短扩展名 + FNV-1a 8 位确定性摘要，防止截断后同名覆盖
    let hash = fnv1a_64(trimmed.as_bytes());
    let (stem, ext) = match trimmed.rfind('.') {
        Some(idx) if idx > 0 && trimmed.len() - idx <= 16 => (&trimmed[..idx], &trimmed[idx..]),
        _ => (trimmed, ""),
    };

    let suffix = format!("~{:08x}{}", hash as u32, ext);
    let budget = MAX_COMPONENT_BYTES.saturating_sub(suffix.len());
    let prefix = truncate_utf8_bytes(stem, budget);
    format!("{}{}", prefix, suffix)
}

fn truncate_utf8_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::{sanitize_filename, Downloader, FetchedSubtitle, MAX_COMPONENT_BYTES};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    #[test]
    fn blocks_traversal_and_special_names() {
        assert_eq!(sanitize_filename(".."), "_");
        assert_eq!(sanitize_filename("."), "_");
        assert_eq!(sanitize_filename("   ...  "), "_");
        assert_eq!(
            sanitize_filename("a/b\\c:d*e?f\"g<h>i|j"),
            "a_b_c_d_e_f_g_h_i_j"
        );
    }

    #[test]
    fn truncates_overlong_cjk_filenames_preserving_extension_and_uniqueness() {
        let base = "ダンジョンに出会いを求めるのは間違っているだろうか".repeat(6);
        let f1 = sanitize_filename(&format!("{}_EP01.ja.ass", base));
        let f2 = sanitize_filename(&format!("{}_EP02.ja.ass", base));
        assert!(f1.len() <= MAX_COMPONENT_BYTES);
        assert!(f2.len() <= MAX_COMPONENT_BYTES);
        assert!(f1.ends_with(".ass"));
        assert!(f2.ends_with(".ass"));
        assert_ne!(f1, f2);
    }

    #[tokio::test]
    async fn streams_atomic_download_and_aborts_oversize_memory_fetch() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = b"Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,Hello\n";

        tokio::spawn(async move {
            for _ in 0..3 {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut req = [0u8; 512];
                let _ = stream.read(&mut req).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.write_all(body).await;
            }
        });

        let dir = std::env::temp_dir().join(format!("jimaku-dl-{}", uuid::Uuid::new_v4()));
        let dl = Downloader {
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            download_path: dir.clone(),
        };
        let url = format!("http://{}/sub.ass", addr);

        // 1) 流式原子下载落盘，且无残留 .part 临时文件
        let out = dl
            .download_subtitle_with_outcome("Test Show", "ep01.ass", &url)
            .await
            .unwrap();
        assert!(!out.skipped_existing);
        assert_eq!(out.size, body.len() as u64);
        assert_eq!(std::fs::read(&out.path).unwrap(), body);
        let entries: Vec<_> = std::fs::read_dir(out.path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(entries.len(), 1);

        // 2) 内存直取：在上限内正常返回字节，超过上限立即返回 TooLarge 且不落盘
        match dl.fetch_subtitle_bytes(&url, 1024).await.unwrap() {
            FetchedSubtitle::Bytes(data) => assert_eq!(data, body),
            FetchedSubtitle::TooLarge => panic!("unexpected TooLarge"),
        }
        assert!(matches!(
            dl.fetch_subtitle_bytes(&url, 10).await.unwrap(),
            FetchedSubtitle::TooLarge
        ));

        std::fs::remove_dir_all(&dir).ok();
    }
}
