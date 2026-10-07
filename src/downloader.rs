use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::fs;
use tracing::{debug, info};

pub struct Downloader {
    client: reqwest::Client,
    download_path: PathBuf,
}

pub struct DownloadOutcome {
    pub path: PathBuf,
    pub skipped_existing: bool,
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

    pub async fn download_subtitle(
        &self,
        entry_name: &str,
        file_name: &str,
        file_url: &str,
    ) -> Result<PathBuf> {
        Ok(self
            .download_subtitle_with_outcome(entry_name, file_name, file_url)
            .await?
            .path)
    }

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

        // 如果文件已存在且大小不为0，跳过下载
        if let Ok(metadata) = fs::metadata(&file_path).await {
            if metadata.len() > 0 {
                debug!("File already exists, skipping: {:?}", file_path);
                return Ok(DownloadOutcome {
                    path: file_path,
                    skipped_existing: true,
                });
            }
        }

        info!("Downloading: {} -> {:?}", file_url, file_path);

        let resp = self
            .client
            .get(file_url)
            .send()
            .await
            .with_context(|| format!("Failed to download from {}", file_url))?;

        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("Download failed with status: {}", status);
        }

        let bytes = resp.bytes().await.context("Failed to read download body")?;

        fs::write(&file_path, &bytes)
            .await
            .with_context(|| format!("Failed to write file: {:?}", file_path))?;

        info!(
            "Downloaded {} ({} bytes) to {:?}",
            safe_file,
            bytes.len(),
            file_path
        );
        Ok(DownloadOutcome {
            path: file_path,
            skipped_existing: false,
        })
    }
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '\0' => '_',
            c => c,
        })
        .collect()
}
