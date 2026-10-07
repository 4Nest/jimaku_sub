//! 消息文本模板与 HTML 工具函数

use crate::database::Subscription;
use crate::jimaku::Entry;
use crate::telegram::NewSubtitle;

/// 新字幕通知卡片（HTML parse mode，所有外部内容需转义）
pub fn new_subtitle_card(sub: &NewSubtitle<'_>) -> String {
    let display_name = sub.english_name.unwrap_or(sub.entry_name);

    let size_mb = sub.file_size as f64 / 1024.0 / 1024.0;
    let size_str = if size_mb < 0.01 {
        format!("{} B", sub.file_size)
    } else {
        format!("{:.2} MB", size_mb)
    };

    let download_status = if sub.downloaded {
        "✅ 已自动下载到本地"
    } else {
        "⬇️ 点击下方按钮发送文件"
    };

    let japanese_line = sub
        .japanese_name
        .filter(|n| !n.is_empty() && *n != display_name)
        .map(|n| format!("🇯🇵 <code>{}</code>\n", html_escape(n)))
        .unwrap_or_default();

    let language_line = detect_language(sub.file_name)
        .map(|lang| format!("🈳 语言: <code>{}</code>\n", lang))
        .unwrap_or_default();

    format!(
        "🎬 <b>新字幕发布</b>\n\n\
        📺 <b>{}</b>\n\
        {}\
        📝 <code>{}</code>\n\
        {}\
        📦 大小: <code>{}</code> · 🕐 {}\n\
        🔗 <a href=\"{}\">下载字幕</a> · <a href=\"https://jimaku.cc/entry/{}\">作品页面</a>\n\n\
        {}",
        html_escape(display_name),
        japanese_line,
        html_escape(sub.file_name),
        language_line,
        size_str,
        sub.file_modified.format("%Y-%m-%d %H:%M UTC"),
        sub.file_url,
        sub.entry_id,
        download_status
    )
}

/// 单个订阅的管理卡片（/listsubs 与按钮操作后刷新用）
pub fn subscription_card(sub: &Subscription) -> String {
    let title = sub.title.as_deref().unwrap_or("未命名订阅");

    let keyword_text = if sub.keywords.is_empty() {
        String::new()
    } else {
        format!(
            "\n🎯 规则: <code>{}</code>",
            html_escape(&sub.keywords.join("|"))
        )
    };

    let notify_state = if sub.muted {
        "🔇 静音"
    } else {
        "🔔 开启"
    };
    let download_state = match sub.auto_download {
        None => "跟随全局",
        Some(true) => "开",
        Some(false) => "关",
    };
    let send_file_state = if sub.send_file { "开" } else { "关" };

    format!(
        "📋 <b>{}</b>\n\
        Entry ID: <code>{}</code>{}\n\
        通知: {} · 自动下载: {} · 发文件: {}\n\
        订阅于 {}",
        html_escape(title),
        sub.entry_id,
        keyword_text,
        notify_state,
        download_state,
        send_file_state,
        sub.created_at.format("%Y-%m-%d %H:%M")
    )
}

/// 从字幕文件名推断语言（启发式，识别不出则不展示）
pub fn detect_language(file_name: &str) -> Option<&'static str> {
    let lower = file_name.to_lowercase();
    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    let has = |candidates: &[&str]| tokens.iter().any(|t| candidates.contains(t));

    if lower.contains('简') || has(&["hans", "chs", "gbk", "gb2312"]) {
        return Some("简体中文");
    }
    if lower.contains('繁') || has(&["hant", "cht", "big5"]) {
        return Some("繁体中文");
    }
    if has(&["jpn", "jp", "japanese"]) || lower.contains('日') {
        return Some("日本語");
    }
    if has(&["eng", "en", "english"]) {
        return Some("English");
    }
    if has(&["kor", "kr", "korean"]) {
        return Some("한국어");
    }
    if has(&["sc"]) {
        return Some("简体中文");
    }
    if has(&["tc"]) {
        return Some("繁体中文");
    }
    None
}

pub fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn display_entry_title(entry: &Entry) -> String {
    entry
        .english_name
        .as_deref()
        .filter(|name| !name.is_empty())
        .unwrap_or(&entry.name)
        .to_string()
}

pub fn format_subscriptions(subs: &[Subscription]) -> String {
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

        let mut flags = String::new();
        if sub.muted {
            flags.push_str(" 🔇");
        }
        if sub.send_file {
            flags.push_str(" 📄");
        }

        lines.push(format!(
            "  • {}{} (<code>{}</code>{}) 订阅于 {}",
            html_escape(title),
            flags,
            sub.entry_id,
            keyword_text,
            sub.created_at.format("%Y-%m-%d %H:%M")
        ));
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::detect_language;

    #[test]
    fn detects_common_language_tags() {
        assert_eq!(detect_language("show.S01E01.zh-Hans.ass"), Some("简体中文"));
        assert_eq!(detect_language("show.S01E01.CHT.zip"), Some("繁体中文"));
        assert_eq!(detect_language("show.EP01.jpn.srt"), Some("日本語"));
        assert_eq!(detect_language("show.EP01.eng.srt"), Some("English"));
        assert_eq!(detect_language("show.EP01.简体.ass"), Some("简体中文"));
        assert_eq!(detect_language("show.EP01.ass"), None);
    }
}
