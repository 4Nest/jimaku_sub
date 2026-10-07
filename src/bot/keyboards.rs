//! Inline keyboard 构造与 callback_data 编解码
//!
//! callback_data 上限 64 字节，统一使用 `<cmd>:<arg>` 短格式：
//! - `dl:<notified_files.id>`  下载字幕文件到聊天

use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackAction {
    /// 下载并发送 notified_files.id 对应的字幕文件
    DownloadFile(i64),
}

pub fn parse_callback(data: &str) -> Option<CallbackAction> {
    let (cmd, arg) = data.split_once(':')?;
    match cmd {
        "dl" => arg.parse().ok().map(CallbackAction::DownloadFile),
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
    fn rejects_unknown_callback() {
        assert_eq!(parse_callback("xx:1"), None);
        assert_eq!(parse_callback("dl:"), None);
        assert_eq!(parse_callback("dl"), None);
    }
}
