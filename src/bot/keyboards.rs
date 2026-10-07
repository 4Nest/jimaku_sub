//! Inline keyboard 构造与 callback_data 编解码
//!
//! callback_data 上限 64 字节，统一使用短格式：
//! - `dl:<file_id>`          下载字幕文件到聊天
//! - `sp:<token>:<idx>`      /sub 搜索结果选择（idx=-1 为取消）
//! - `mute:<entry_id>`       切换订阅静音
//! - `adl:<entry_id>`        循环切换自动下载（跟随全局→开→关）
//! - `sfl:<entry_id>`        切换通知后发送文件
//! - `uns:<entry_id>`        请求退订（二次确认）
//! - `unsy:<entry_id>`       确认退订
//! - `unsn:<entry_id>`       取消退订

use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

use crate::database::Subscription;
use crate::jimaku::Entry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackAction {
    /// 下载并发送 notified_files.id 对应的字幕文件
    DownloadFile(i64),
    /// 选择 /sub 搜索结果中的第 idx 个候选
    SelectEntry {
        token: String,
        idx: i64,
    },
    ToggleMute(i64),
    CycleAutoDownload(i64),
    ToggleSendFile(i64),
    UnsubAsk(i64),
    UnsubConfirm(i64),
    UnsubCancel(i64),
}

pub fn parse_callback(data: &str) -> Option<CallbackAction> {
    let (cmd, arg) = data.split_once(':').unwrap_or((data, ""));
    match cmd {
        "dl" => arg.parse().ok().map(CallbackAction::DownloadFile),
        "sp" => {
            let (token, idx) = arg.split_once(':')?;
            Some(CallbackAction::SelectEntry {
                token: token.to_string(),
                idx: idx.parse().ok()?,
            })
        }
        "mute" => arg.parse().ok().map(CallbackAction::ToggleMute),
        "adl" => arg.parse().ok().map(CallbackAction::CycleAutoDownload),
        "sfl" => arg.parse().ok().map(CallbackAction::ToggleSendFile),
        "uns" => arg.parse().ok().map(CallbackAction::UnsubAsk),
        "unsy" => arg.parse().ok().map(CallbackAction::UnsubConfirm),
        "unsn" => arg.parse().ok().map(CallbackAction::UnsubCancel),
        _ => None,
    }
}

/// 新字幕通知卡片的「⬇️ 下载」按钮
pub fn download_keyboard(file_id: i64) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "⬇️ 下载字幕文件",
        format!("dl:{}", file_id),
    )]])
}

/// /sub 搜索结果的候选列表按钮
pub fn entry_selection_keyboard(token: &str, entries: &[Entry]) -> InlineKeyboardMarkup {
    let mut rows: Vec<Vec<InlineKeyboardButton>> = entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| {
            let title = entry.english_name.as_deref().unwrap_or(&entry.name);
            let label = truncate(&format!("{} ({})", title, entry.id), 48);
            vec![InlineKeyboardButton::callback(
                label,
                format!("sp:{}:{}", token, idx),
            )]
        })
        .collect();
    rows.push(vec![InlineKeyboardButton::callback(
        "❌ 取消",
        format!("sp:{}:-1", token),
    )]);
    InlineKeyboardMarkup::new(rows)
}

/// 单个订阅的管理面板按钮
pub fn subscription_panel(sub: &Subscription) -> InlineKeyboardMarkup {
    let mute_label = if sub.muted {
        "🔔 开启通知"
    } else {
        "🔇 静音"
    };
    let auto_download_label = match sub.auto_download {
        None => "📥 自动下载: 跟随全局",
        Some(true) => "📥 自动下载: 开",
        Some(false) => "📥 自动下载: 关",
    };
    let send_file_label = if sub.send_file {
        "📄 发文件: 开"
    } else {
        "📄 发文件: 关"
    };

    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            mute_label,
            format!("mute:{}", sub.entry_id),
        )],
        vec![InlineKeyboardButton::callback(
            auto_download_label,
            format!("adl:{}", sub.entry_id),
        )],
        vec![InlineKeyboardButton::callback(
            send_file_label,
            format!("sfl:{}", sub.entry_id),
        )],
        vec![InlineKeyboardButton::callback(
            "❌ 退订",
            format!("uns:{}", sub.entry_id),
        )],
    ])
}

/// 退订二次确认
pub fn unsub_confirm_keyboard(entry_id: i64) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("✅ 确认退订", format!("unsy:{}", entry_id)),
        InlineKeyboardButton::callback("↩️ 取消", format!("unsn:{}", entry_id)),
    ]])
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::{parse_callback, CallbackAction};

    #[test]
    fn parses_download_callback() {
        assert_eq!(
            parse_callback("dl:42"),
            Some(CallbackAction::DownloadFile(42))
        );
    }

    #[test]
    fn parses_select_entry_callback() {
        assert_eq!(
            parse_callback("sp:ab12cd34:2"),
            Some(CallbackAction::SelectEntry {
                token: "ab12cd34".to_string(),
                idx: 2
            })
        );
        assert_eq!(
            parse_callback("sp:ab12cd34:-1"),
            Some(CallbackAction::SelectEntry {
                token: "ab12cd34".to_string(),
                idx: -1
            })
        );
    }

    #[test]
    fn parses_panel_callbacks() {
        assert_eq!(
            parse_callback("mute:7"),
            Some(CallbackAction::ToggleMute(7))
        );
        assert_eq!(
            parse_callback("adl:7"),
            Some(CallbackAction::CycleAutoDownload(7))
        );
        assert_eq!(
            parse_callback("sfl:7"),
            Some(CallbackAction::ToggleSendFile(7))
        );
        assert_eq!(parse_callback("uns:7"), Some(CallbackAction::UnsubAsk(7)));
        assert_eq!(
            parse_callback("unsy:7"),
            Some(CallbackAction::UnsubConfirm(7))
        );
        assert_eq!(
            parse_callback("unsn:7"),
            Some(CallbackAction::UnsubCancel(7))
        );
    }

    #[test]
    fn rejects_unknown_callback() {
        assert_eq!(parse_callback("xx:1"), None);
        assert_eq!(parse_callback("dl:"), None);
        assert_eq!(parse_callback("dl"), None);
    }
}
