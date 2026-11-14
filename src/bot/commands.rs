//! Bot 命令定义与 handler

use teloxide::prelude::*;
use teloxide::types::ParseMode;
use teloxide::utils::command::BotCommands;
use tracing::{error, warn};

use crate::bot::keyboards::{entry_selection_keyboard, subscription_panel};
use crate::bot::text::{display_entry_title, format_subscriptions, html_escape, subscription_card};
use crate::bot::AppState;
use crate::database::Subscription;
/// /sub 多结果选择的短 token（callback_data 有 64 字节上限）
pub fn new_search_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

#[allow(unreachable_code)]
mod cmd_def {
    use teloxide::utils::command::BotCommands;

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
}
pub use cmd_def::Command;

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
            if state.scheduler.is_checking() {
                bot.send_message(chat_id, "⏳ 当前已有检查任务正在运行，请稍候")
                    .await?;
                return Ok(());
            }

            bot.send_message(chat_id, "🔍 正在触发检查...").await?;

            match state.scheduler.try_check_now(&state.cancel).await {
                Ok(true) => {
                    bot.send_message(chat_id, "✅ 检查完成").await?;
                }
                Ok(false) => {
                    bot.send_message(chat_id, "⏳ 当前已有检查任务正在运行，请稍候")
                        .await?;
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
            Ok(subs) if subs.is_empty() => {
                bot.send_message(chat_id, "📭 当前没有动态订阅").await?;
            }
            Ok(subs) => {
                const MAX_PANELS: usize = 20;
                for (idx, sub) in subs.iter().take(MAX_PANELS).enumerate() {
                    if idx > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                    }
                    bot.send_message(chat_id, subscription_card(sub))
                        .parse_mode(ParseMode::Html)
                        .reply_markup(subscription_panel(sub))
                        .await?;
                }
                if subs.len() > MAX_PANELS {
                    bot.send_message(
                        chat_id,
                        format!(
                            "…其余 {} 个订阅请用 /unsub <entry_id> 管理",
                            subs.len() - MAX_PANELS
                        ),
                    )
                    .await?;
                }
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

    let downloaded_urls = match state.db.get_downloaded_urls(entry_id).await {
        Ok(urls) => urls,
        Err(e) => {
            bot.send_message(chat_id, format!("❌ 查询下载记录失败: {}", e))
                .await?;
            return Ok(());
        }
    };

    let mut downloaded = 0;
    let mut skipped = 0;
    let mut failed = 0;
    let mut failures = Vec::new();

    for file in files {
        if downloaded_urls.contains(&file.url) {
            skipped += 1;
            continue;
        }

        match state
            .downloader
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
            Ok(entries) if entries.len() > 1 => {
                // 多结果：暂存候选项，用按钮让用户选择
                const MAX_CANDIDATES: usize = 8;
                let token = new_search_token();
                let candidates: Vec<_> = entries.into_iter().take(MAX_CANDIDATES).collect();
                let entry_ids: Vec<i64> = candidates.iter().map(|e| e.id).collect();

                match state
                    .db
                    .create_pending_search(&token, chat_id.0, &parsed.release_keywords, &entry_ids)
                    .await
                {
                    Ok(_) => {
                        bot.send_message(
                            chat_id,
                            format!(
                                "🔍 找到 <b>{}</b> 个匹配作品，请选择要订阅的:",
                                candidates.len()
                            ),
                        )
                        .parse_mode(ParseMode::Html)
                        .reply_markup(entry_selection_keyboard(&token, &candidates))
                        .await?;
                    }
                    Err(e) => {
                        bot.send_message(chat_id, format!("❌ 暂存搜索结果失败: {}", e))
                            .await?;
                    }
                }
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

    // 1) 按数字 entry_id 精确退订
    if let Ok(entry_id) = query.parse::<i64>() {
        let existing = state.db.get_subscription(entry_id).await.ok().flatten();
        match state.db.remove_subscription(entry_id).await {
            Ok(true) => {
                let label = existing
                    .and_then(|s| s.title)
                    .map(|t| format!("<b>{}</b> (<code>{}</code>)", html_escape(&t), entry_id))
                    .unwrap_or_else(|| format!("<code>{}</code>", entry_id));
                bot.send_message(chat_id, format!("✅ 已取消订阅: {}", label))
                    .parse_mode(ParseMode::Html)
                    .await?;
            }
            Ok(false) => {
                bot.send_message(chat_id, format!("ℹ️ 没找到订阅: <code>{}</code>", entry_id))
                    .parse_mode(ParseMode::Html)
                    .await?;
            }
            Err(e) => {
                bot.send_message(chat_id, format!("❌ 取消订阅失败: {}", e))
                    .await?;
            }
        }
        return Ok(());
    }

    // 2) 按标题在本地已订阅列表中匹配（精确优先，其次唯一子串匹配，多命中时列出防误删）
    let subs = match state.db.list_subscriptions().await {
        Ok(subs) => subs,
        Err(e) => {
            bot.send_message(chat_id, format!("❌ 查询订阅失败: {}", e))
                .await?;
            return Ok(());
        }
    };

    let mut matched = match_local_subscriptions(&subs, query);

    // 3) 本地未命中时（如用日文原名搜英文标题订阅），搜索 Jimaku 并与本地已订阅集合求交集
    if matched.is_empty() && !subs.is_empty() {
        match state.jimaku.search_entries_by_query(query).await {
            Ok(entries) => {
                let found_ids: std::collections::HashSet<i64> =
                    entries.into_iter().map(|e| e.id).collect();
                matched = subs
                    .iter()
                    .filter(|s| found_ids.contains(&s.entry_id))
                    .cloned()
                    .collect();
            }
            Err(e) => {
                error!("Failed to resolve unsubscribe query {}: {}", query, e);
            }
        }
    }

    match matched.len() {
        0 => {
            bot.send_message(
                chat_id,
                format!("ℹ️ 没找到订阅: <code>{}</code>", html_escape(query)),
            )
            .parse_mode(ParseMode::Html)
            .await?;
        }
        1 => {
            let target = &matched[0];
            match state.db.remove_subscription(target.entry_id).await {
                Ok(true) => {
                    let title = target.title.as_deref().unwrap_or(query);
                    bot.send_message(
                        chat_id,
                        format!(
                            "✅ 已取消订阅: <b>{}</b> (<code>{}</code>)",
                            html_escape(title),
                            target.entry_id
                        ),
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
        }
        _ => {
            let list = matched
                .iter()
                .map(|s| {
                    format!(
                        "  • {} → <code>/unsub {}</code>",
                        html_escape(s.title.as_deref().unwrap_or("未命名订阅")),
                        s.entry_id
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            bot.send_message(
                chat_id,
                format!("⚠️ 匹配到多个已订阅作品，请用 Entry ID 精确退订:\n{}", list),
            )
            .parse_mode(ParseMode::Html)
            .await?;
        }
    }

    Ok(())
}

fn match_local_subscriptions(subs: &[Subscription], query: &str) -> Vec<Subscription> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }

    let exact: Vec<Subscription> = subs
        .iter()
        .filter(|s| {
            s.title
                .as_deref()
                .is_some_and(|t| t.trim().to_lowercase() == q)
        })
        .cloned()
        .collect();
    if !exact.is_empty() {
        return exact;
    }

    subs.iter()
        .filter(|s| {
            s.title
                .as_deref()
                .is_some_and(|t| t.to_lowercase().contains(&q))
        })
        .cloned()
        .collect()
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
    use super::{match_local_subscriptions, parse_sub_args};
    use crate::database::Subscription;
    use chrono::Utc;
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

    #[test]
    fn local_unsub_matching_prefers_exact_and_flags_ambiguous_partials() {
        let mk = |id: i64, title: &str| Subscription {
            entry_id: id,
            title: Some(title.to_string()),
            keywords: vec![],
            created_at: Utc::now(),
            muted: false,
            auto_download: None,
            send_file: false,
        };
        let subs = vec![mk(1, "Slime"), mk(2, "Slime Season 2")];
        // 精确命中第 1 部，不会把第 2 部也算作歧义
        let exact = match_local_subscriptions(&subs, "slime");
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].entry_id, 1);

        // 仅子串命中多部时全部返回，由上层提示精确 ID 防误删
        let subs2 = vec![mk(1, "Slime Season 1"), mk(2, "Slime Season 2")];
        let partial = match_local_subscriptions(&subs2, "slime");
        assert_eq!(partial.len(), 2);
    }
}
