mod config;
mod database;
mod downloader;
mod jimaku;
mod scheduler;
mod telegram;

use std::{fs, sync::Arc};

use anyhow::{Context, Result};
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use teloxide::utils::command::BotCommands;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::config::{Config, LoggingConfig};
use crate::database::Database;
use crate::downloader::Downloader;
use crate::jimaku::JimakuClient;
use crate::scheduler::Scheduler;
use crate::telegram::TelegramNotifier;

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Jimaku 字幕订阅 Bot 命令:\n")]
enum Command {
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

#[derive(Clone)]
struct AppState {
    db: Arc<Database>,
    scheduler: Arc<Mutex<Option<Arc<Scheduler>>>>,
    config: Config,
    jimaku: Arc<JimakuClient>,
    allowed_chat_id: ChatId,
}

use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load().context("Failed to load configuration")?;
    init_logging(&config.logging).context("Failed to initialize logging")?;

    info!("Starting Jimaku Subscriber...");
    info!("Configuration loaded successfully");

    let db = Arc::new(
        Database::new(&config.database.url)
            .await
            .context("Failed to initialize database")?,
    );

    let jimaku = JimakuClient::new(&config.jimaku.base_url, &config.jimaku.api_key)
        .context("Failed to create Jimaku client")?;
    let command_jimaku = Arc::new(
        JimakuClient::new(&config.jimaku.base_url, &config.jimaku.api_key)
            .context("Failed to create command Jimaku client")?,
    );

    let notifier = TelegramNotifier::new(&config.telegram.bot_token, &config.telegram.chat_id)
        .context("Failed to create Telegram notifier")?;

    let scheduler = Arc::new(Scheduler::new(
        config.clone(),
        jimaku,
        db.clone(),
        notifier.clone(),
    )?);

    let state = AppState {
        db,
        scheduler: Arc::new(Mutex::new(Some(scheduler.clone()))),
        config,
        jimaku: command_jimaku,
        allowed_chat_id: notifier.chat_id(),
    };

    if let Err(e) = backfill_subscription_titles(&state).await {
        error!("Failed to backfill subscription titles: {}", e);
    }

    // 发送启动通知
    let _ = notifier
        .send_message(format!(
            "🚀 <b>Jimaku 字幕订阅服务已启动</b>\n\n\
            ⏰ 检查间隔: {} 秒\n\
            ⬇️ 自动下载: {}\n\
            📁 下载路径: <code>{}</code>",
            state.config.scheduler.interval_seconds,
            if state.config.download.enabled {
                "开启"
            } else {
                "关闭"
            },
            state.config.download.download_path
        ))
        .await;

    // dispatcher 使用独立的无限流 Bot，避免通知队列阻塞轮询
    let bot = Bot::new(&state.config.telegram.bot_token);
    let handler = Update::filter_message()
        .filter_command::<Command>()
        .endpoint(answer);

    info!("Starting Telegram bot dispatcher...");

    let cancel = CancellationToken::new();

    // 启动调度器任务
    let scheduler_clone = scheduler.clone();
    let scheduler_cancel = cancel.clone();
    let mut scheduler_handle = tokio::spawn(async move {
        if let Err(e) = scheduler_clone.run(scheduler_cancel).await {
            error!("Scheduler error: {}", e);
        }
    });

    // 启动 Bot dispatcher
    let mut dispatcher = Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .build();
    let dispatcher_shutdown = dispatcher.shutdown_token();
    let mut bot_handle = tokio::spawn(async move {
        dispatcher.dispatch().await;
    });

    tokio::select! {
        _ = &mut scheduler_handle => {
            error!("Scheduler task ended unexpectedly, exiting for restart");
            std::process::exit(1);
        }
        _ = &mut bot_handle => {
            error!("Bot task ended unexpectedly, exiting for restart");
            std::process::exit(1);
        }
        _ = shutdown_signal() => {
            info!("Shutdown signal received, stopping gracefully...");
            cancel.cancel();
            if let Ok(shutdown) = dispatcher_shutdown.shutdown() {
                shutdown.await;
            }
            let _ = scheduler_handle.await;
            let _ = bot_handle.await;
            info!("Shutdown complete");
        }
    }

    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => {
            // 无法注册 SIGTERM 时退化为只监听 Ctrl-C
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

fn init_logging(config: &LoggingConfig) -> Result<()> {
    fs::create_dir_all(&config.dir).context("Failed to create log directory")?;
    cleanup_old_logs(&config.dir, config.retention_days);

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,teloxide=warn"));
    let file_appender = tracing_appender::rolling::daily(&config.dir, "jimaku-subscriber.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    Box::leak(Box::new(guard));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer())
        .with(fmt::layer().with_ansi(false).with_writer(file_writer))
        .init();

    Ok(())
}

/// 删除超过保留天数的日志文件（文件名格式 jimaku-subscriber.log.YYYY-MM-DD）
fn cleanup_old_logs(log_dir: &str, retention_days: u32) {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(retention_days as i64);
    let entries = match fs::read_dir(log_dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(date_str) = name.strip_prefix("jimaku-subscriber.log.") else {
            continue;
        };
        let Ok(date) = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d") else {
            continue;
        };
        let Some(datetime) = date.and_hms_opt(0, 0, 0) else {
            continue;
        };
        if datetime < cutoff.naive_utc() {
            let _ = fs::remove_file(entry.path());
        }
    }
}

async fn answer(bot: Bot, msg: Message, cmd: Command, state: AppState) -> ResponseResult<()> {
    let chat_id = msg.chat.id;
    if chat_id != state.allowed_chat_id {
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
        Command::Status => {
            let notified = state.db.get_notified_files_count().await.unwrap_or(0);
            let downloaded = state.db.get_downloaded_count().await.unwrap_or(0);
            let last_check = state.db.get_last_check_time().await.unwrap_or(None);
            let subs = state.db.list_subscriptions().await.unwrap_or_default();
            let subs_str = format_subscriptions(&subs);

            let last_check_str = match last_check {
                Some(ts) => {
                    let dt = chrono::DateTime::from_timestamp(ts, 0)
                        .map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                        .unwrap_or_else(|| "未知".to_string());
                    dt
                }
                None => "尚未检查".to_string(),
            };

            let text = format!(
                "📊 <b>服务状态</b>\n\n\
                🔔 已通知字幕: <code>{}</code>\n\
                ⬇️ 已下载字幕: <code>{}</code>\n\
                ⏰ 上次检查: <code>{}</code>\n\n\
                📋 <b>动态订阅</b>\n{}",
                notified, downloaded, last_check_str, subs_str
            );

            bot.send_message(chat_id, text)
                .parse_mode(ParseMode::Html)
                .await?;
        }
        Command::Checknow => {
            bot.send_message(chat_id, "🔍 正在触发检查...").await?;

            let scheduler_opt = state.scheduler.lock().await;
            if let Some(scheduler) = scheduler_opt.as_ref() {
                match scheduler.check_once().await {
                    Ok(_) => {
                        bot.send_message(chat_id, "✅ 检查完成").await?;
                    }
                    Err(e) => {
                        bot.send_message(chat_id, format!("❌ 检查失败: {}", e))
                            .await?;
                    }
                }
            } else {
                bot.send_message(chat_id, "❌ 调度器未初始化").await?;
            }
        }
        Command::Download(args) => handle_download(&bot, chat_id, &state, &args).await?,
        Command::Sub(args) => handle_subscribe(&bot, chat_id, &state, &args).await?,
        Command::Unsub(args) => handle_unsubscribe(&bot, chat_id, &state, &args).await?,
        Command::Listsubs => {
            let subs = state.db.list_subscriptions().await.unwrap_or_default();
            let subs_str = format_subscriptions(&subs);
            let text = if subs.is_empty() {
                "📭 当前没有动态订阅".to_string()
            } else {
                format!("📋 <b>动态订阅列表</b>\n\n{}", subs_str)
            };

            bot.send_message(chat_id, text)
                .parse_mode(ParseMode::Html)
                .await?;
        }
    }

    Ok(())
}

fn format_subscriptions(subs: &[crate::database::Subscription]) -> String {
    if subs.is_empty() {
        return "无动态订阅".to_string();
    }

    let mut lines = Vec::with_capacity(subs.len());
    for sub in subs {
        let title = sub.title.as_deref().unwrap_or("未命名订阅");

        let keyword_text = if sub.keywords.is_empty() {
            String::new()
        } else {
            format!(
                " / 规则: <code>{}</code>",
                html_escape(&sub.keywords.join("|"))
            )
        };

        lines.push(format!(
            "  • {} (<code>{}</code>{}) 订阅于 {}",
            html_escape(title),
            sub.entry_id,
            keyword_text,
            sub.created_at.format("%Y-%m-%d %H:%M")
        ));
    }

    lines.join("\n")
}

async fn backfill_subscription_titles(state: &AppState) -> Result<()> {
    let subs = state.db.list_subscriptions().await?;
    for sub in subs {
        if let Ok(entry) = state.jimaku.get_entry_by_id(sub.entry_id).await {
            state
                .db
                .update_subscription_title(sub.entry_id, &display_entry_title(&entry))
                .await?;
        }
    }

    Ok(())
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

fn display_entry_title(entry: &crate::jimaku::Entry) -> String {
    entry
        .english_name
        .as_deref()
        .filter(|name| !name.is_empty())
        .unwrap_or(&entry.name)
        .to_string()
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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
