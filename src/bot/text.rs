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

// ---------- 频道全量推送 ----------

/// sendDocument caption 的硬上限（Telegram Bot API）
const CAPTION_LIMIT: usize = 1024;
/// 单行内容截断长度，保证多行组合后仍低于 CAPTION_LIMIT
const LINE_LIMIT: usize = 200;

/// 频道推送一条字幕所需的展示数据
pub struct ChannelSubtitle<'a> {
    pub japanese_name: Option<&'a str>,
    pub english_name: Option<&'a str>,
    /// AniList 罗马音；查不到时调用方降级为 entry.name（本身即罗马音）
    pub romaji: &'a str,
    pub file_name: &'a str,
    pub file_size: i64,
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

/// 引用块名字行：罗马音 / 英文 顺序，去空白并做大小写不敏感去重
fn name_lines(sub: &ChannelSubtitle<'_>) -> Vec<(&'static str, String)> {
    let mut lines: Vec<(&'static str, String)> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for (emoji, name) in [
        ("🔤", Some(sub.romaji)),
        ("🇬🇧", sub.english_name),
        ("🇯🇵", sub.japanese_name),
    ] {
        let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
            continue;
        };
        let key = name.to_lowercase();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        lines.push((emoji, name.to_string()));
    }
    lines
}

/// 主标题：优先日语名，其次罗马音，再英语名
fn channel_primary<'a>(sub: &ChannelSubtitle<'a>) -> &'a str {
    sub.japanese_name
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .or_else(|| {
            let romaji = sub.romaji.trim();
            if romaji.is_empty() {
                None
            } else {
                Some(romaji)
            }
        })
        .or_else(|| sub.english_name.map(str::trim).filter(|n| !n.is_empty()))
        .unwrap_or("未知作品")
}

/// 标题 + 引用块（方案 A · 引用块杂志风）。
/// 返回 (标题行, 引用块) ；引用块为主标题之外的名字行，为空则不渲染。
fn channel_header(sub: &ChannelSubtitle<'_>) -> (String, String) {
    let primary = channel_primary(sub);
    let primary_key = primary.to_lowercase();
    let title = format!(
        "📺 <b><a href=\"https://jimaku.cc/entry/{}\">{}</a></b>",
        sub.entry_id,
        html_escape(truncate(primary, LINE_LIMIT))
    );

    let quote_lines: Vec<String> = name_lines(sub)
        .into_iter()
        .filter(|(_, name)| name.to_lowercase() != primary_key)
        .map(|(emoji, name)| format!("{} {}", emoji, html_escape(truncate(&name, LINE_LIMIT))))
        .collect();
    let quote = if quote_lines.is_empty() {
        String::new()
    } else {
        format!("<blockquote>{}</blockquote>", quote_lines.join("\n"))
    };
    (title, quote)
}

/// meta 行：大小 · 时间 · AniList 文字链接（有 id 时）
fn channel_meta(sub: &ChannelSubtitle<'_>) -> String {
    let mut meta = format!(
        "📦 {} · 🕐 {}",
        format_size(sub.file_size),
        sub.file_modified.format("%Y-%m-%d %H:%M UTC")
    );
    if let Some(id) = sub.anilist_id {
        meta.push_str(&format!(
            " · 🎬 <a href=\"https://anilist.co/anime/{}\">AniList</a>",
            id
        ));
    }
    meta
}

/// 频道文件消息的 Caption（HTML，≤1024 字符）
pub fn channel_caption(sub: &ChannelSubtitle<'_>) -> String {
    let (title, quote) = channel_header(sub);
    let mut text = title;
    if !quote.is_empty() {
        text.push_str(&format!("\n{}\n", quote));
    }
    text.push_str(&format!(
        "\n📝 <code>{}</code>\n{}",
        html_escape(truncate(sub.file_name, LINE_LIMIT)),
        channel_meta(sub)
    ));
    // Telegram 上限按字符计（Rust len() 是字节数，CJK 会虚高 3 倍）
    debug_assert!(
        text.chars().count() <= CAPTION_LIMIT,
        "caption exceeds limit: {} chars",
        text.chars().count()
    );
    text
}

/// 超过发送上限的文件：频道文字卡片（下载走消息下方 URL 按钮）
pub fn channel_link_card(sub: &ChannelSubtitle<'_>) -> String {
    let (title, quote) = channel_header(sub);
    let mut text = title;
    if !quote.is_empty() {
        text.push_str(&format!("\n{}\n", quote));
    }
    text.push_str(&format!(
        "\n📝 <code>{}</code>\n{}\n⚠️ 文件超过发送上限，请用下方按钮下载",
        html_escape(sub.file_name),
        channel_meta(sub)
    ));
    text
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

    fn sub<'a>(ja: Option<&'a str>, en: Option<&'a str>, romaji: &'a str) -> ChannelSubtitle<'a> {
        ChannelSubtitle {
            japanese_name: ja,
            english_name: en,
            romaji,
            file_name: "S01E13.WEBRip.TVer.ja[cc].srt",
            file_size: 45_000,
            entry_id: 11783,
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
    fn caption_magazine_layout_with_quote_block() {
        let s = sub(
            Some("本好きの下剋上 領主の養女"),
            Some("Ascendance of a Bookworm"),
            "Honzuki no Gekokujou",
        );
        let c = channel_caption(&s);
        // 主标题：日语名粗体 + 跳转 jimaku 作品页
        assert!(c.contains(
            "📺 <b><a href=\"https://jimaku.cc/entry/11783\">本好きの下剋上 領主の養女</a></b>"
        ));
        // 引用块：罗马音 / 英文
        assert!(c.contains(
            "<blockquote>🔤 Honzuki no Gekokujou\n🇬🇧 Ascendance of a Bookworm</blockquote>"
        ));
        // 元信息行：大小 · 时间 · AniList 文字链接
        assert!(c.contains(
            "📦 44 KB · 🕐 2026-10-07 15:20 UTC · 🎬 <a href=\"https://anilist.co/anime/999999\">AniList</a>"
        ));
        // Caption 里不放文字下载链接（走 URL 按钮）
        assert!(!c.contains("下载字幕"));
    }

    #[test]
    fn caption_dedups_same_names_case_insensitive() {
        // 英语名与罗马音相同（大小写不同）→ 英文名行被去重
        let s = sub(None, Some("Yomi no Tsugai"), "yomi no tsugai");
        let c = channel_caption(&s);
        assert!(!c.contains("🇬🇧"), "英文行应被去重: {}", c);
        assert!(c.contains(">yomi no tsugai</a></b>"));
        // 罗马音=主标题（无日语名时），引用块为空
        assert!(!c.contains("<blockquote>"));
    }

    #[test]
    fn caption_all_names_same_keeps_title_only() {
        let s = sub(Some("Same Name"), Some("Same Name"), "same name");
        let c = channel_caption(&s);
        assert!(c.contains(">Same Name</a></b>"));
        assert!(!c.contains("<blockquote>"));
    }

    #[test]
    fn caption_handles_missing_names() {
        let s = sub(None, None, "Only Romaji");
        let c = channel_caption(&s);
        assert!(c.contains(">Only Romaji</a></b>"));
        assert!(!c.contains("🇯🇵") && !c.contains("🇬🇧"));
    }

    #[test]
    fn caption_omits_anilist_link_without_id() {
        let mut s = sub(None, None, "Only Romaji");
        s.anilist_id = None;
        let c = channel_caption(&s);
        assert!(!c.contains("AniList"));
    }

    #[test]
    fn caption_stays_within_telegram_limit() {
        let long = "あ".repeat(500);
        let long2 = "貴".repeat(500);
        let long3 = "族".repeat(500);
        let s = ChannelSubtitle {
            japanese_name: Some(&long),
            english_name: Some(&long2),
            romaji: &long3,
            file_name: &long,
            file_size: 1,
            entry_id: 1,
            anilist_id: Some(1),
            file_modified: chrono::Utc::now(),
        };
        // 三行不同名 + 长文件名全部拉满，仍须在字符上限内
        assert!(channel_caption(&s).chars().count() <= 1024);
    }

    #[test]
    fn caption_escapes_html() {
        let s = sub(None, Some("A & B <C>"), "A & B <C>");
        let c = channel_caption(&s);
        assert!(c.contains("A &amp; B &lt;C&gt;"));
    }

    #[test]
    fn link_card_contains_oversize_hint() {
        let s = sub(Some("日"), Some("En"), "Ro");
        let c = channel_link_card(&s);
        assert!(c.contains("jimaku.cc/entry/11783"));
        assert!(c.contains("超过发送上限"));
        assert!(c.contains("<blockquote>"));
    }
}
