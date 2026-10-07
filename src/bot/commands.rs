//! Bot 命令定义与 handler

use teloxide::prelude::*;
use teloxide::types::ParseMode;
use teloxide::utils::command::BotCommands;
use tracing::{error, warn};

use crate::bot::text::{display_entry_title, format_subscriptions, html_escape};
use crate::bot::AppState;
use crate::downloader::Downloader;

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Jimaku 字幕订阅 Bot 命令:\n")]
pub enum Command {
    #[command(description = "显示帮助信息")]
    Help,
    #[command(description = "查看服务状态")]
    Status,
    #[command(description = "立即触发一次检查")]
    Checknow,
    #[command(description = "下载指定 entry 的全部字幕 /download <entry_id>")]
    Download(String),
    #[command(
        rename = "sub",
        alias = "subscribe",
        hide_aliases,
        description = "添加订阅 /sub <entry_id|作品名> [-r NF|Netflix|ATX]"
    )]
    Sub(String),
    #[command(
        rename = "unsub",
        alias = "unsubscribe",
        hide_aliases,
        description = "取消订阅 /unsub <作品名|entry_id>"
    )]
    Unsub(String),
    #[command(description = "列出当前订阅")]
    Listsubs,
}

pub async fn answer(bot: Bot, msg: Message, cmd: Command, state: AppState) -> ResponseResult<()> {
    let chat_id = msg.chat.id;
    if !state.is_allowed(chat_id) {
        warn!(
            "Rejected Telegram command from unauthorized chat_id: {}",
            chat_id
        );
        bot.send_message(chat_id, "⛔ 无权限使用此 Bot").await?;
        return Ok(());
    }

    match cmd {
        Command::Help => {
            bot.send_message(chat_id, Command::descriptions().to_string())
                .await?;
        }
        Command::Status => match build_status_text(&state).await {
            Ok(text) => {
                bot.send_message(chat_id, text)
                    .parse_mode(ParseMode::Html)
                    .await?;
            }
            Err(e) => {
                error!("Failed to build status: {}", e);
                bot.send_message(chat_id, format!("❌ 状态查询失败: {}", e))
                    .await?;
            }
        },
        Command::Checknow => {
            bot.send_message(chat_id, "🔍 正在触发检查...").await?;

            match state.scheduler.check_once().await {
                Ok(_) => {
                    bot.send_message(chat_id, "✅ 检查完成").await?;
                }
                Err(e) => {
                    bot.send_message(chat_id, format!("❌ 检查失败: {}", e))
                        .await?;
                }
            }
        }
        Command::Download(args) => handle_download(&bot, chat_id, &state, &args).await?,
        Command::Sub(args) => handle_subscribe(&bot, chat_id, &state, &args).await?,
        Command::Unsub(args) => handle_unsubscribe(&bot, chat_id, &state, &args).await?,
        Command::Listsubs => match state.db.list_subscriptions().await {
            Ok(subs) => {
                let text = if subs.is_empty() {
                    "📭 当前没有动态订阅".to_string()
                } else {
                    format!("📋 <b>动态订阅列表</b>\n\n{}", format_subscriptions(&subs))
                };

                bot.send_message(chat_id, text)
                    .parse_mode(ParseMode::Html)
                    .await?;
            }
            Err(e) => {
                error!("Failed to list subscriptions: {}", e);
                bot.send_message(chat_id, format!("❌ 订阅列表查询失败: {}", e))
                    .await?;
            }
        },
    }

    Ok(())
}

async fn build_status_text(state: &AppState) -> anyhow::Result<String> {
    let notified = state.db.get_notified_files_count().await?;
    let downloaded = state.db.get_downloaded_count().await?;
    let last_check = state.db.get_last_check_time().await?;
    let subs = state.db.list_subscriptions().await?;
    let subs_str = format_subscriptions(&subs);

    let last_check_str = match last_check {
        Some(ts) => chrono::DateTime::from_timestamp(ts, 0)
            .map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| "未知".to_string()),
        None => "尚未检查".to_string(),
    };

    Ok(format!(
        "📊 <b>服务状态</b>\n\n\
        🔔 已通知字幕: <code>{}</code>\n\
        ⬇️ 已下载字幕: <code>{}</code>\n\
        ⏰ 上次检查: <code>{}</code>\n\n\
        📋 <b>动态订阅</b>\n{}",
        notified, downloaded, last_check_str, subs_str
    ))
}

async fn handle_download(
    bot: &Bot,
    chat_id: ChatId,
    state: &AppState,
    args: &str,
) -> ResponseResult<()> {
    let entry_id = match args.trim().parse::<i64>() {
        Ok(entry_id) => entry_id,
        Err(_) => {
            bot.send_message(chat_id, "用法: /download <entry_id>")
                .await?;
            return Ok(());
        }
    };

    bot.send_message(
        chat_id,
        format!("⬇️ 正在下载 Entry <code>{}</code> 的全部字幕...", entry_id),
    )
    .parse_mode(ParseMode::Html)
    .await?;

    let entry = match state.jimaku.get_entry_by_id(entry_id).await {
        Ok(entry) => entry,
        Err(e) => {
            bot.send_message(chat_id, format!("❌ 查询 Jimaku entry 失败: {}", e))
                .await?;
            return Ok(());
        }
    };

    let files = match state.jimaku.get_entry_files(entry_id).await {
        Ok(files) => files,
        Err(e) => {
            bot.send_message(chat_id, format!("❌ 查询字幕文件失败: {}", e))
                .await?;
            return Ok(());
        }
    };

    if files.is_empty() {
        bot.send_message(
            chat_id,
            format!(
                "ℹ️ <b>{}</b> 没有可下载的字幕文件",
                html_escape(&display_entry_title(&entry))
            ),
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Ok(());
    }

    let downloader = Downloader::new(&state.config.download.download_path);
    let mut downloaded = 0;
    let mut skipped = 0;
    let mut failed = 0;
    let mut failures = Vec::new();

    for file in files {
        match state.db.is_file_downloaded(&file.url).await {
            Ok(true) => {
                skipped += 1;
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                failed += 1;
                failures.push(format!("{}: {}", file.name, e));
                continue;
            }
        }

        match downloader
            .download_subtitle_with_outcome(&entry.name, &file.name, &file.url)
            .await
        {
            Ok(outcome) => {
                if outcome.skipped_existing {
                    skipped += 1;
                } else {
                    downloaded += 1;
                }

                if let Err(e) = state
                    .db
                    .record_file(
                        entry.id,
                        &entry.name,
                        &file.name,
                        &file.url,
                        Some(file.size),
                        true,
                    )
                    .await
                {
                    failed += 1;
                    failures.push(format!("{}: {}", file.name, e));
                }
            }
            Err(e) => {
                failed += 1;
                failures.push(format!("{}: {}", file.name, e));
            }
        }
    }

    let mut text = format!(
        "✅ <b>{}</b> 下载完成\n\
        Entry ID: <code>{}</code>\n\
        新下载: <code>{}</code>\n\
        已跳过: <code>{}</code>\n\
        失败: <code>{}</code>\n\
        路径: <code>{}</code>",
        html_escape(&display_entry_title(&entry)),
        entry.id,
        downloaded,
        skipped,
        failed,
        html_escape(&state.config.download.download_path)
    );

    if !failures.is_empty() {
        let preview = failures
            .iter()
            .take(5)
            .map(|failure| format!("  • {}", html_escape(failure)))
            .collect::<Vec<_>>()
            .join("\n");
        text.push_str("\n\n失败明细:\n");
        text.push_str(&preview);
        if failures.len() > 5 {
            text.push_str("\n  • ...");
        }
    }

    bot.send_message(chat_id, text)
        .parse_mode(ParseMode::Html)
        .await?;

    Ok(())
}

async fn handle_subscribe(
    bot: &Bot,
    chat_id: ChatId,
    state: &AppState,
    args: &str,
) -> ResponseResult<()> {
    let parsed = match parse_sub_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            bot.send_message(chat_id, message).await?;
            return Ok(());
        }
    };

    let entry = if let Some(entry_id) = parsed.entry_id {
        match state.jimaku.get_entry_by_id(entry_id).await {
            Ok(entry) => entry,
            Err(e) => {
                bot.send_message(chat_id, format!("❌ 查询 Jimaku entry 失败: {}", e))
                    .await?;
                return Ok(());
            }
        }
    } else {
        match state.jimaku.search_entries_by_query(&parsed.query).await {
            Ok(entries) if entries.is_empty() => {
                bot.send_message(
                    chat_id,
                    format!(
                        "❌ Jimaku 没找到作品: <code>{}</code>",
                        html_escape(&parsed.query)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await?;
                return Ok(());
            }
            Ok(mut entries) => entries.remove(0),
            Err(e) => {
                bot.send_message(chat_id, format!("❌ 查询 Jimaku 失败: {}", e))
                    .await?;
                return Ok(());
            }
        }
    };

    match state
        .db
        .add_subscription_details(
            entry.id,
            Some(&display_entry_title(&entry)),
            &parsed.release_keywords,
        )
        .await
    {
        Ok(_) => {
            let rules = if parsed.release_keywords.is_empty() {
                "无".to_string()
            } else {
                html_escape(&parsed.release_keywords.join("|"))
            };

            bot.send_message(
                chat_id,
                format!(
                    "✅ 已订阅: <b>{}</b>\nEntry ID: <code>{}</code>\n规则: <code>{}</code>",
                    html_escape(&display_entry_title(&entry)),
                    entry.id,
                    rules
                ),
            )
            .parse_mode(ParseMode::Html)
            .await?;
        }
        Err(e) => {
            bot.send_message(chat_id, format!("❌ 添加订阅失败: {}", e))
                .await?;
        }
    }

    Ok(())
}

async fn handle_unsubscribe(
    bot: &Bot,
    chat_id: ChatId,
    state: &AppState,
    args: &str,
) -> ResponseResult<()> {
    let query = args.trim();
    if query.is_empty() {
        bot.send_message(chat_id, "用法: /unsub <作品名|entry_id>")
            .await?;
        return Ok(());
    }

    let removed = if let Ok(entry_id) = query.parse::<i64>() {
        state.db.remove_subscription(entry_id).await
    } else {
        match state.db.remove_subscription_by_name(query).await {
            Ok(true) => Ok(true),
            Ok(false) => match state.jimaku.search_entries_by_query(query).await {
                Ok(entries) if entries.is_empty() => Ok(false),
                Ok(entries) => state.db.remove_subscription(entries[0].id).await,
                Err(e) => {
                    error!("Failed to resolve unsubscribe query {}: {}", query, e);
                    Ok(false)
                }
            },
            Err(e) => Err(e),
        }
    };

    match removed {
        Ok(true) => {
            bot.send_message(
                chat_id,
                format!("✅ 已取消订阅: <code>{}</code>", html_escape(query)),
            )
            .parse_mode(ParseMode::Html)
            .await?;
        }
        Ok(false) => {
            bot.send_message(
                chat_id,
                format!("ℹ️ 没找到订阅: <code>{}</code>", html_escape(query)),
            )
            .parse_mode(ParseMode::Html)
            .await?;
        }
        Err(e) => {
            bot.send_message(chat_id, format!("❌ 取消订阅失败: {}", e))
                .await?;
        }
    }

    Ok(())
}

struct ParsedSubArgs {
    query: String,
    entry_id: Option<i64>,
    release_keywords: Vec<String>,
}

fn parse_sub_args(args: &str) -> std::result::Result<ParsedSubArgs, &'static str> {
    let args = args.trim();
    if args.is_empty() {
        return Err("用法: /sub <entry_id|作品名> [-r NF|Netflix|ATX]");
    }

    let (query_part, rule_part) = split_release_rule(args);
    let release_keywords = rule_part.map(parse_release_keywords).unwrap_or_default();
    let query = query_part.trim().to_string();

    if query.is_empty() {
        return Err("❌ 作品名不能为空");
    }

    if let Ok(entry_id) = query.parse::<i64>() {
        return Ok(ParsedSubArgs {
            query: format!("Entry {}", entry_id),
            entry_id: Some(entry_id),
            release_keywords,
        });
    }

    Ok(ParsedSubArgs {
        query,
        entry_id: None,
        release_keywords,
    })
}

fn split_release_rule(args: &str) -> (&str, Option<&str>) {
    if let Some((query, rule)) = args.split_once(" -r ") {
        return (query, Some(rule));
    }

    if let Some((query, rule)) = args.split_once(" --release ") {
        return (query, Some(rule));
    }

    (args, None)
}

fn parse_release_keywords(rule: &str) -> Vec<String> {
    rule.split('|')
        .map(str::trim)
        .filter(|keyword| !keyword.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::parse_sub_args;

    #[test]
    fn parses_entry_subscription() {
        let parsed = parse_sub_args("11824").unwrap();
        assert_eq!(parsed.entry_id, Some(11824));
        assert!(parsed.release_keywords.is_empty());
    }

    #[test]
    fn parses_title_subscription_with_release_keywords() {
        let parsed = parse_sub_args("Tongari Boushi no Atelier -r NF|Netflix|ATX").unwrap();
        assert_eq!(parsed.query, "Tongari Boushi no Atelier");
        assert_eq!(parsed.entry_id, None);
        assert_eq!(parsed.release_keywords, vec!["NF", "Netflix", "ATX"]);
    }

    #[test]
    fn treats_entry_prefix_as_title_text() {
        let parsed = parse_sub_args("entry 11824").unwrap();
        assert_eq!(parsed.query, "entry 11824");
        assert_eq!(parsed.entry_id, None);
    }
}
