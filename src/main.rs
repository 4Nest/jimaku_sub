mod bot;
mod config;
mod database;
mod downloader;
mod jimaku;
mod scheduler;
mod telegram;

use std::{fs, sync::Arc};

use anyhow::{Context, Result};
use teloxide::prelude::*;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::bot::callbacks::handle_callback;
use crate::bot::commands::{answer, Command};
use crate::bot::AppState;
use crate::config::{Config, LoggingConfig};
use crate::database::Database;
use crate::jimaku::JimakuClient;
use crate::scheduler::Scheduler;
use crate::telegram::TelegramNotifier;

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

    let scheduler_jimaku = JimakuClient::new(&config.jimaku.base_url, &config.jimaku.api_key)
        .context("Failed to create Jimaku client")?;
    let command_jimaku = Arc::new(
        JimakuClient::new(&config.jimaku.base_url, &config.jimaku.api_key)
            .context("Failed to create command Jimaku client")?,
    );

    let notifier = TelegramNotifier::new(&config.telegram.bot_token, &config.telegram.chat_id)
        .context("Failed to create Telegram notifier")?;

    let scheduler = Arc::new(Scheduler::new(
        config.clone(),
        scheduler_jimaku,
        db.clone(),
        notifier.clone(),
    ));

    let state = AppState {
        db,
        scheduler: scheduler.clone(),
        config,
        jimaku: command_jimaku,
        allowed_chat_id: notifier.chat_id(),
    };

    bot::backfill_or_log(&state).await;

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
    let dispatcher_bot = Bot::new(&state.config.telegram.bot_token);
    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .endpoint(answer),
        )
        .branch(Update::filter_callback_query().endpoint(handle_callback));

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
    let mut dispatcher = Dispatcher::builder(dispatcher_bot, handler)
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
