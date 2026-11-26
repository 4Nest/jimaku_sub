//! Callback query（inline 按钮）处理

use teloxide::prelude::*;
use teloxide::types::{CallbackQuery, InlineKeyboardMarkup, InputFile, ParseMode};
use tracing::{error, info, warn};

use crate::bot::keyboards::{
    parse_callback, subscription_panel, unsub_confirm_keyboard, CallbackAction,
};
use crate::bot::text::{display_entry_title, html_attr_escape, html_escape, subscription_card};
use crate::bot::AppState;
use crate::database::Subscription;
use crate::downloader::{DownloadOutcome, FetchedSubtitle};
use crate::goindex::GOINDEX_ENTRY_ID;
use crate::scheduler::MAX_DOCUMENT_SIZE;

pub async fn handle_callback(
    bot: Bot,
    query: CallbackQuery,
    state: AppState,
) -> ResponseResult<()> {
    let Some(message) = query.message.as_ref() else {
        bot.answer_callback_query(&query.id).await?;
        return Ok(());
    };
    if !state.is_allowed(message.chat().id) {
        bot.answer_callback_query(&query.id)
            .text("⛔ 无权限使用此 Bot")
            .await?;
        return Ok(());
    }

    let Some(data) = query.data.as_deref() else {
        bot.answer_callback_query(&query.id).await?;
        return Ok(());
    };
    match parse_callback(data) {
        Some(CallbackAction::DownloadFile(file_id)) => {
            handle_download_file(&bot, &query, &state, file_id).await?;
        }
        Some(CallbackAction::SelectEntry { token, idx }) => {
            handle_select_entry(&bot, &query, &state, &token, idx).await?;
        }
        Some(CallbackAction::ToggleMute(entry_id)) => {
            mutate_policy(&bot, &query, &state, entry_id, |sub| sub.muted = !sub.muted).await?;
        }
        Some(CallbackAction::CycleAutoDownload(entry_id)) => {
            mutate_policy(&bot, &query, &state, entry_id, |sub| {
                sub.auto_download = match sub.auto_download {
                    None => Some(true),
                    Some(true) => Some(false),
                    Some(false) => None,
                };
            })
            .await?;
        }
        Some(CallbackAction::ToggleSendFile(entry_id)) => {
            mutate_policy(&bot, &query, &state, entry_id, |sub| {
                sub.send_file = !sub.send_file;
            })
            .await?;
        }
        Some(CallbackAction::UnsubAsk(entry_id)) => {
            bot.answer_callback_query(&query.id).await?;
            bot.edit_message_reply_markup(message.chat().id, message.id())
                .reply_markup(unsub_confirm_keyboard(entry_id))
                .await?;
        }
        Some(CallbackAction::UnsubConfirm(entry_id)) => {
            bot.answer_callback_query(&query.id).await?;
            handle_unsub_confirm(&bot, &query, &state, entry_id).await?;
        }
        Some(CallbackAction::UnsubCancel(entry_id)) => {
            bot.answer_callback_query(&query.id).await?;
            // 恢复该订阅的面板
            if let Ok(Some(sub)) = state.db.get_subscription(entry_id).await {
                refresh_panel(&bot, &query, &sub).await;
            }
        }
        None => {
            bot.answer_callback_query(&query.id).await?;
        }
    }

    Ok(())
}

/// 修改订阅策略并原地刷新面板
async fn mutate_policy(
    bot: &Bot,
    query: &CallbackQuery,
    state: &AppState,
    entry_id: i64,
    mutate: impl FnOnce(&mut Subscription),
) -> ResponseResult<()> {
    let sub = match state.db.get_subscription(entry_id).await {
        Ok(Some(sub)) => sub,
        Ok(None) => {
            bot.answer_callback_query(&query.id)
                .text("⚠️ 订阅不存在")
                .await?;
            return Ok(());
        }
        Err(e) => {
            error!("Failed to load subscription {}: {}", entry_id, e);
            bot.answer_callback_query(&query.id)
                .text("❌ 查询失败，请稍后重试")
                .await?;
            return Ok(());
        }
    };

    let mut sub = sub;
    mutate(&mut sub);

    if let Err(e) = state
        .db
        .save_subscription_policy(entry_id, sub.muted, sub.auto_download, sub.send_file)
        .await
    {
        error!("Failed to save subscription policy {}: {}", entry_id, e);
        bot.answer_callback_query(&query.id)
            .text("❌ 保存失败，请稍后重试")
            .await?;
        return Ok(());
    }

    bot.answer_callback_query(&query.id).await?;
    refresh_panel(bot, query, &sub).await;
    Ok(())
}

/// 用最新订阅状态原地更新面板消息（忽略「内容未变化」等编辑错误）
async fn refresh_panel(bot: &Bot, query: &CallbackQuery, sub: &Subscription) {
    let Some(message) = query.message.as_ref() else {
        return;
    };
    if let Err(e) = bot
        .edit_message_text(message.chat().id, message.id(), subscription_card(sub))
        .parse_mode(ParseMode::Html)
        .reply_markup(subscription_panel(sub))
        .await
    {
        tracing::debug!("Failed to refresh subscription panel: {}", e);
    }
}

/// /sub 多结果选择
async fn handle_select_entry(
    bot: &Bot,
    query: &CallbackQuery,
    state: &AppState,
    token: &str,
    idx: i64,
) -> ResponseResult<()> {
    let Some(message) = query.message.as_ref() else {
        return Ok(());
    };
    let chat_id = message.chat().id;
    bot.answer_callback_query(&query.id).await?;

    let pending = match state.db.get_pending_search(token).await {
        Ok(Some(pending)) => pending,
        _ => {
            edit_plain(bot, query, "⚠️ 该搜索已过期，请重新使用 /sub <作品名>").await;
            return Ok(());
        }
    };

    if pending.chat_id != chat_id.0 {
        return Ok(());
    }

    // 取消选择
    if idx < 0 {
        let _ = state.db.delete_pending_search(token).await;
        edit_plain(bot, query, "已取消订阅").await;
        return Ok(());
    }

    let Some(&entry_id) = pending.entry_ids.get(idx as usize) else {
        edit_plain(bot, query, "⚠️ 选择无效，请重新使用 /sub <作品名>").await;
        return Ok(());
    };

    let entry = match state.jimaku.get_entry_by_id(entry_id).await {
        Ok(entry) => entry,
        Err(e) => {
            error!("Failed to fetch entry {} for selection: {}", entry_id, e);
            edit_plain(bot, query, "❌ 查询 Jimaku entry 失败，请稍后重试").await;
            return Ok(());
        }
    };

    if let Err(e) = state
        .db
        .add_subscription_details(
            entry.id,
            Some(&display_entry_title(&entry)),
            &pending.keywords,
        )
        .await
    {
        error!("Failed to add subscription {}: {}", entry.id, e);
        edit_plain(bot, query, "❌ 添加订阅失败，请稍后重试").await;
        return Ok(());
    }

    let _ = state.db.delete_pending_search(token).await;

    // 显示订阅成功 + 策略面板
    if let Ok(Some(sub)) = state.db.get_subscription(entry.id).await {
        if let Err(e) = bot
            .edit_message_text(
                chat_id,
                message.id(),
                format!("✅ 已订阅\n\n{}", subscription_card(&sub)),
            )
            .parse_mode(ParseMode::Html)
            .reply_markup(subscription_panel(&sub))
            .await
        {
            tracing::debug!("Failed to edit selection message: {}", e);
        }
    }

    Ok(())
}

async fn handle_unsub_confirm(
    bot: &Bot,
    query: &CallbackQuery,
    state: &AppState,
    entry_id: i64,
) -> ResponseResult<()> {
    let title = state
        .db
        .get_subscription(entry_id)
        .await
        .ok()
        .flatten()
        .and_then(|sub| sub.title)
        .unwrap_or_else(|| entry_id.to_string());

    match state.db.remove_subscription(entry_id).await {
        Ok(true) => {
            edit_plain(bot, query, &format!("✅ 已退订: {}", title)).await;
        }
        Ok(false) => {
            edit_plain(bot, query, "ℹ️ 该订阅已不存在").await;
        }
        Err(e) => {
            error!("Failed to remove subscription {}: {}", entry_id, e);
            bot.answer_callback_query(&query.id)
                .text("❌ 退订失败，请稍后重试")
                .await?;
        }
    }

    Ok(())
}

/// 把面板消息替换为纯文本终态（移除按钮）
async fn edit_plain(bot: &Bot, query: &CallbackQuery, text: &str) {
    let Some(message) = query.message.as_ref() else {
        return;
    };
    if let Err(e) = bot
        .edit_message_text(message.chat().id, message.id(), text)
        .reply_markup(InlineKeyboardMarkup::default())
        .await
    {
        tracing::debug!("Failed to edit message: {}", e);
    }
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

    if record
        .file_size
        .is_some_and(|s| s > 0 && (s as u64) > MAX_DOCUMENT_SIZE)
    {
        send_oversize_hint(bot, chat_id, &record.file_name, &record.file_url).await?;
        return Ok(());
    }

    let local = state
        .downloader
        .local_path(&record.entry_name, &record.file_name);

    // 1) 本地已有完好文件则直接发送
    if let Ok(meta) = tokio::fs::metadata(&local).await {
        if meta.len() > 0 {
            if meta.len() > MAX_DOCUMENT_SIZE {
                send_oversize_hint(bot, chat_id, &record.file_name, &record.file_url).await?;
                return Ok(());
            }
            if let Err(e) = bot.send_document(chat_id, InputFile::file(&local)).await {
                error!("Failed to send document {:?}: {}", local, e);
                bot.send_message(chat_id, "❌ 文件发送失败，请稍后重试")
                    .await?;
            }
            return Ok(());
        }
    }

    // 2) 判断是否需要落盘保存（遵循订阅级 auto_download 或全局 download.enabled）
    let persist_to_disk = state
        .db
        .get_subscription(record.entry_id)
        .await
        .ok()
        .flatten()
        .and_then(|sub| sub.auto_download)
        .unwrap_or(state.config.download.enabled);

    if persist_to_disk {
        match download_with_goindex_fallback(state, &record, file_id).await {
            Ok((outcome, effective_url)) => {
                let _ = state
                    .db
                    .record_file(
                        record.entry_id,
                        &record.entry_name,
                        &record.file_name,
                        &effective_url,
                        record.file_size,
                        true,
                    )
                    .await;
                if outcome.size > MAX_DOCUMENT_SIZE {
                    send_oversize_hint(bot, chat_id, &record.file_name, &effective_url).await?;
                    return Ok(());
                }
                if let Err(e) = bot
                    .send_document(chat_id, InputFile::file(&outcome.path))
                    .await
                {
                    error!("Failed to send document {:?}: {}", outcome.path, e);
                    bot.send_message(chat_id, "❌ 文件发送失败，请稍后重试")
                        .await?;
                }
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
            }
        }
    } else {
        match fetch_bytes_with_goindex_fallback(state, &record, file_id).await {
            Ok((FetchedSubtitle::Bytes(data), _)) => {
                let input = InputFile::memory(data).file_name(record.file_name.clone());
                if let Err(e) = bot.send_document(chat_id, input).await {
                    error!("Failed to send memory document {}: {}", record.file_name, e);
                    bot.send_message(chat_id, "❌ 文件发送失败，请稍后重试")
                        .await?;
                }
            }
            Ok((FetchedSubtitle::TooLarge, effective_url)) => {
                send_oversize_hint(bot, chat_id, &record.file_name, &effective_url).await?;
            }
            Err(e) => {
                error!("Failed to fetch subtitle {}: {}", record.file_name, e);
                bot.send_message(
                    chat_id,
                    format!(
                        "❌ 下载失败: <code>{}</code>",
                        html_escape(&record.file_name)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await?;
            }
        }
    }

    Ok(())
}

/// 下载到本地：GoIndex 文件原 URL 失效（如被归档移入日期子目录或 `old/`）时自动寻址重试并回写最新 URL
async fn download_with_goindex_fallback(
    state: &AppState,
    record: &crate::database::NotifiedFile,
    file_id: i64,
) -> anyhow::Result<(DownloadOutcome, String)> {
    let first_err = match state
        .downloader
        .download_subtitle_with_outcome(&record.entry_name, &record.file_name, &record.file_url)
        .await
    {
        Ok(outcome) => return Ok((outcome, record.file_url.clone())),
        Err(e) => e,
    };

    if record.entry_id != GOINDEX_ENTRY_ID {
        return Err(first_err);
    }
    let Some(ref goindex) = state.goindex else {
        return Err(first_err);
    };

    warn!(
        "GoIndex download failed for {} ({}), trying archived paths",
        record.file_name, first_err
    );

    let candidates = goindex.candidate_archive_urls(&record.file_name, &record.file_url);
    for url in &candidates {
        if let Ok(outcome) = state
            .downloader
            .download_subtitle_with_outcome(&record.entry_name, &record.file_name, url)
            .await
        {
            info!("Recovered moved GoIndex subtitle via {}", url);
            let _ = state.db.update_notified_file_url(file_id, url).await;
            return Ok((outcome, url.clone()));
        }
    }

    if let Ok(Some(found_url)) = goindex.find_file_url_by_name(&record.file_name).await {
        if found_url != record.file_url && !candidates.contains(&found_url) {
            if let Ok(outcome) = state
                .downloader
                .download_subtitle_with_outcome(&record.entry_name, &record.file_name, &found_url)
                .await
            {
                info!("Recovered moved GoIndex subtitle by scan: {}", found_url);
                let _ = state.db.update_notified_file_url(file_id, &found_url).await;
                return Ok((outcome, found_url));
            }
        }
    }

    Err(first_err)
}

/// 内存直取字幕：GoIndex 文件原 URL 失效（如被归档移入日期子目录或 `old/`）时自动寻址重试并回写最新 URL
async fn fetch_bytes_with_goindex_fallback(
    state: &AppState,
    record: &crate::database::NotifiedFile,
    file_id: i64,
) -> anyhow::Result<(FetchedSubtitle, String)> {
    let first_err = match state
        .downloader
        .fetch_subtitle_bytes(&record.file_url, MAX_DOCUMENT_SIZE as usize)
        .await
    {
        Ok(fetched) => return Ok((fetched, record.file_url.clone())),
        Err(e) => e,
    };

    if record.entry_id != GOINDEX_ENTRY_ID {
        return Err(first_err);
    }
    let Some(ref goindex) = state.goindex else {
        return Err(first_err);
    };

    warn!(
        "GoIndex fetch failed for {} ({}), trying archived paths",
        record.file_name, first_err
    );

    let candidates = goindex.candidate_archive_urls(&record.file_name, &record.file_url);
    for url in &candidates {
        if let Ok(fetched) = state
            .downloader
            .fetch_subtitle_bytes(url, MAX_DOCUMENT_SIZE as usize)
            .await
        {
            info!("Recovered moved GoIndex subtitle via {}", url);
            let _ = state.db.update_notified_file_url(file_id, url).await;
            return Ok((fetched, url.clone()));
        }
    }

    if let Ok(Some(found_url)) = goindex.find_file_url_by_name(&record.file_name).await {
        if found_url != record.file_url && !candidates.contains(&found_url) {
            if let Ok(fetched) = state
                .downloader
                .fetch_subtitle_bytes(&found_url, MAX_DOCUMENT_SIZE as usize)
                .await
            {
                info!("Recovered moved GoIndex subtitle by scan: {}", found_url);
                let _ = state.db.update_notified_file_url(file_id, &found_url).await;
                return Ok((fetched, found_url));
            }
        }
    }

    Err(first_err)
}

async fn send_oversize_hint(
    bot: &Bot,
    chat_id: ChatId,
    file_name: &str,
    file_url: &str,
) -> ResponseResult<()> {
    bot.send_message(
        chat_id,
        format!(
            "⚠️ <code>{}</code> 超过 Telegram 发送上限 (48MB)，请<a href=\"{}\">点击直链下载</a>",
            html_escape(file_name),
            html_attr_escape(file_url)
        ),
    )
    .parse_mode(ParseMode::Html)
    .await?;
    Ok(())
}
