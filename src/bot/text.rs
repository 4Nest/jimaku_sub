//! 消息文本模板与 HTML 工具函数

use crate::database::Subscription;
use crate::jimaku::Entry;
use crate::telegram::NewSubtitle;

/// 新字幕通知卡片（HTML parse mode，所有外部内容需转义）
pub fn new_subtitle_card(sub: &NewSubtitle<'_>) -> String {
    let display_name = sub.english_name.unwrap_or(sub.entry_name);

    let size_str = format_size(sub.file_size);

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
        format_time(sub.file_modified),
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

/// 文件大小格式化（B / KB / MB）
pub fn format_size(bytes: i64) -> String {
    if bytes < 1024 {
        return format!("{} B", bytes);
    }
    let mb = bytes as f64 / 1024.0 / 1024.0;
    if mb < 1.0 {
        return format!("{:.0} KB", bytes as f64 / 1024.0);
    }
    format!("{:.2} MB", mb)
}

/// 时间统一按 UTC+8 显示
pub fn format_time(dt: chrono::DateTime<chrono::Utc>) -> String {
    let cst = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
    dt.with_timezone(&cst).format("%Y-%m-%d %H:%M").to_string()
}

// ---------- 频道全量推送 ----------

/// sendDocument caption 的硬上限（Telegram Bot API）
const CAPTION_LIMIT: usize = 1024;
/// 单行内容截断长度，保证多行组合后仍低于 CAPTION_LIMIT
const LINE_LIMIT: usize = 200;

/// 频道推送一条字幕所需的展示数据
pub struct ChannelSubtitle<'a> {
    pub japanese_name: Option<&'a str>,
    pub file_name: &'a str,
    pub file_size: i64,
    /// jimaku 下载直链（仅超大文件链接卡片使用）
    pub file_url: &'a str,
    pub entry_id: i64,
    /// 有 id 时在 meta 行附加 AniList 文字链接
    pub anilist_id: Option<i32>,
    pub file_modified: chrono::DateTime<chrono::Utc>,
}

/// 按 chars() 截断，避免切坏 UTF-8
fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

// ---------- MarkdownV2 工具（频道推送使用） ----------

/// MarkdownV2 普通文本转义：_*[]()~`>#+-=|{}.! 全部加反斜杠
fn md_escape(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if "_*[]()~`>#+-=|{}.!".contains(c) {
                vec!['\\', c]
            } else {
                vec![c]
            }
        })
        .collect()
}

/// MarkdownV2 等宽行内代码：只需转义反斜杠和反引号
fn md_code(text: &str) -> String {
    format!("`{}`", text.replace('\\', "\\\\").replace('`', "\\`"))
}

/// MarkdownV2 文字链接
fn md_link(label: &str, url: &str) -> String {
    format!(
        "[{}]({})",
        md_escape(label),
        url.replace('\\', "\\\\").replace(')', "\\)")
    )
}

/// 频道消息正文（MarkdownV2）：等宽文件名 + meta 行 + 日语番名 hashtag
fn channel_body(sub: &ChannelSubtitle<'_>) -> String {
    let mut text = format!(
        "🎞 {}\n📦 {} │ 🕐 {}",
        md_code(truncate(sub.file_name, LINE_LIMIT)),
        md_escape(&format_size(sub.file_size)),
        md_escape(&format_time(sub.file_modified))
    );

    text.push_str(&format!(
        " │ 🔗 {}",
        md_link(
            "jimaku",
            &format!("https://jimaku.cc/entry/{}", sub.entry_id)
        )
    ));

    if let Some(id) = sub.anilist_id {
        text.push_str(&format!(
            " │ 🎬 {}",
            md_link("AniList", &format!("https://anilist.co/anime/{}", id))
        ));
    }

    // 日语番名 hashtag：标签只认文字/数字/下划线，空格和标点（！等）会被截断，直接剔除
    // 前面空一行，与 meta 行视觉上隔开
    if let Some(tag) = sub
        .japanese_name
        .map(sanitize_hashtag)
        .filter(|t| !t.is_empty())
    {
        text.push_str(&format!("\n\n\\#{}", md_escape(truncate(&tag, LINE_LIMIT))));
    }
    text
}

/// 净化 hashtag：仅保留字母数字（含 CJK）和下划线
fn sanitize_hashtag(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

/// 频道文件消息的 Caption（MarkdownV2，≤1024 字符）
pub fn channel_caption(sub: &ChannelSubtitle<'_>) -> String {
    let text = channel_body(sub);
    // Telegram 上限按字符计（Rust len() 是字节数，CJK 会虚高 3 倍）
    debug_assert!(
        text.chars().count() <= CAPTION_LIMIT,
        "caption exceeds limit: {} chars",
        text.chars().count()
    );
    text
}

/// 超过发送上限的文件：频道文字卡片（附 jimaku 下载直链）
pub fn channel_link_card(sub: &ChannelSubtitle<'_>) -> String {
    format!(
        "{}\n⚠️ {}：{}",
        channel_body(sub),
        md_escape("文件超过发送上限，请从 jimaku 下载"),
        md_link("点击下载", sub.file_url)
    )
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
    use super::{
        channel_caption, channel_link_card, detect_language, format_size, ChannelSubtitle,
    };

    fn sub(ja: Option<&str>) -> ChannelSubtitle<'_> {
        ChannelSubtitle {
            japanese_name: ja,
            file_name: "S01E13.WEBRip.TVer.ja[cc].srt",
            file_size: 45_000,
            entry_id: 11783,
            file_url: "https://jimaku.cc/file/1",
            anilist_id: Some(999999),
            file_modified: chrono::DateTime::parse_from_rfc3339("2026-10-07T15:20:00+00:00")
                .unwrap()
                .with_timezone(&chrono::Utc),
        }
    }

    #[test]
    fn detects_common_language_tags() {
        assert_eq!(detect_language("show.S01E01.zh-Hans.ass"), Some("简体中文"));
        assert_eq!(detect_language("show.S01E01.CHT.zip"), Some("繁体中文"));
        assert_eq!(detect_language("show.EP01.jpn.srt"), Some("日本語"));
        assert_eq!(detect_language("show.EP01.eng.srt"), Some("English"));
        assert_eq!(detect_language("show.EP01.简体.ass"), Some("简体中文"));
        assert_eq!(detect_language("show.EP01.ass"), None);
    }

    #[test]
    fn formats_size() {
        assert_eq!(format_size(500), "500 B");
        assert_eq!(format_size(45_000), "44 KB");
        assert_eq!(format_size(5_242_880), "5.00 MB");
    }

    #[test]
    fn caption_layout_markdown() {
        let s = sub(Some("本好きの下剋上 領主の養女"));
        let c = channel_caption(&s);
        // 等宽文件名开头，无标题行和分隔线
        assert!(c.starts_with("🎞 `S01E13.WEBRip.TVer.ja[cc].srt`"));
        assert!(!c.contains("✦"));
        assert!(!c.contains("──────"));
        // meta 行：竖线分隔 + jimaku / AniList 链接，日期连字符已转义
        assert!(c.contains(
            "📦 44 KB │ 🕐 2026\\-10\\-07 23:20 │ 🔗 [jimaku](https://jimaku.cc/entry/11783) │ 🎬 [AniList](https://anilist.co/anime/999999)"
        ));
        // 日语番名 hashtag：空一行 + 空格剔除、不带标点
        assert!(c.contains("\n\n\\#本好きの下剋上領主の養女"));
        assert!(!c.contains("下载字幕"));
    }

    #[test]
    fn caption_hashtag_strips_punctuation() {
        let s = sub(Some("生徒会にも穴はある！"));
        let c = channel_caption(&s);
        assert!(
            c.contains("\n\n\\#生徒会にも穴はある"),
            "hashtag 不正确: {}",
            c
        );
        assert!(!c.contains("\\#生徒会にも穴はある！"));
    }

    #[test]
    fn caption_omits_hashtag_without_japanese_name() {
        let s = sub(None);
        let c = channel_caption(&s);
        assert!(!c.contains("\\#"));
    }

    #[test]
    fn caption_omits_anilist_link_without_id() {
        let mut s = sub(None);
        s.anilist_id = None;
        let c = channel_caption(&s);
        assert!(!c.contains("AniList"));
        // jimaku 链接仍保留
        assert!(c.contains("[jimaku](https://jimaku.cc/entry/11783)"));
    }

    #[test]
    fn caption_stays_within_telegram_limit() {
        let long = "あ".repeat(500);
        let s = ChannelSubtitle {
            japanese_name: Some(&long),
            file_name: &long,
            file_size: 1,
            entry_id: 1,
            file_url: "https://jimaku.cc/file/1",
            anilist_id: Some(1),
            file_modified: chrono::Utc::now(),
        };
        // 长文件名 + 长标签全部拉满，仍须在字符上限内
        assert!(channel_caption(&s).chars().count() <= 1024);
    }

    #[test]
    fn code_span_keeps_punctuation_unescaped() {
        // 等宽代码内只需转义反引号和反斜杠，. 和 [] 原样保留
        let s = sub(None);
        let c = channel_caption(&s);
        assert!(c.contains("`S01E13.WEBRip.TVer.ja[cc].srt`"));
    }

    #[test]
    fn link_card_contains_oversize_hint() {
        let s = sub(Some("日"));
        let c = channel_link_card(&s);
        assert!(c.contains("jimaku.cc/entry/11783"));
        assert!(c.contains("超过发送上限"));
        assert!(c.contains("\\#日"));
    }
}
