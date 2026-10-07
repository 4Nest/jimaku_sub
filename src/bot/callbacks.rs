//! Callback query（inline 按钮）处理

use teloxide::prelude::*;
use teloxide::types::{CallbackQuery, InputFile, ParseMode};
use tracing::error;

use crate::bot::keyboards::{parse_callback, CallbackAction};
use crate::bot::text::html_escape;
use crate::bot::AppState;
use crate::downloader::Downloader;

pub async fn handle_callback(
    bot: Bot,
    query: CallbackQuery,
    state: AppState,
) -> ResponseResult<()> {
    if let Some(message) = query.message.as_ref() {
        if !state.is_allowed(message.chat().id) {
            bot.answer_callback_query(&query.id)
                .text("⛔ 无权限使用此 Bot")
                .await?;
            return Ok(());
        }
    }

    let Some(data) = query.data.as_deref() else {
        return Ok(());
    };

    match parse_callback(data) {
        Some(CallbackAction::DownloadFile(file_id)) => {
            handle_download_file(&bot, &query, &state, file_id).await?;
        }
        None => {
            bot.answer_callback_query(&query.id).await?;
        }
    }

    Ok(())
}

/// 「⬇️ 下载」按钮：把字幕文件作为 Telegram 文档发送到聊天
async fn handle_download_file(
    bot: &Bot,
    query: &CallbackQuery,
    state: &AppState,
    file_id: i64,
) -> ResponseResult<()> {
    let Some(message) = query.message.as_ref() else {
        return Ok(());
    };
    let chat_id = message.chat().id;

    let record = match state.db.get_notified_file_by_id(file_id).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            bot.answer_callback_query(&query.id)
                .text("⚠️ 字幕记录不存在或已过期")
                .await?;
            return Ok(());
        }
        Err(e) => {
            error!("Failed to query notified file {}: {}", file_id, e);
            bot.answer_callback_query(&query.id)
                .text("❌ 查询失败，请稍后重试")
                .await?;
            return Ok(());
        }
    };

    bot.answer_callback_query(&query.id)
        .text("⏳ 正在准备文件...")
        .await?;

    let downloader = Downloader::new(&state.config.download.download_path);
    let local = downloader.local_path(&record.entry_name, &record.file_name);

    // 本地已有文件直接用，否则现下载
    let path = match tokio::fs::metadata(&local).await {
        Ok(meta) if meta.len() > 0 => local,
        _ => match downloader
            .download_subtitle(&record.entry_name, &record.file_name, &record.file_url)
            .await
        {
            Ok(path) => {
                let _ = state
                    .db
                    .record_file(
                        record.entry_id,
                        &record.entry_name,
                        &record.file_name,
                        &record.file_url,
                        record.file_size,
                        true,
                    )
                    .await;
                path
            }
            Err(e) => {
                error!("Failed to download subtitle {}: {}", record.file_name, e);
                bot.send_message(
                    chat_id,
                    format!(
                        "❌ 下载失败: <code>{}</code>",
                        html_escape(&record.file_name)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await?;
                return Ok(());
            }
        },
    };

    if let Err(e) = bot.send_document(chat_id, InputFile::file(&path)).await {
        error!("Failed to send document {:?}: {}", path, e);
        bot.send_message(chat_id, "❌ 文件发送失败，请稍后重试")
            .await?;
    }

    Ok(())
}
