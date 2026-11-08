//! Identify links belonging to the connected Jira instance and render issue references
//! as compact, escaped Pango links. No network or GTK widgets are needed here.

use gtk::glib;
use reqwest::Url;

fn valid_key(key: &str) -> bool {
    let Some((project, number)) = key.split_once('-') else {
        return false;
    };
    project.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
        && project.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        && !number.is_empty()
        && number.bytes().all(|b| b.is_ascii_digit())
}

/// Only intercept issue links on the currently connected Jira site and context
/// path. A link to another Jira server must still open in the web browser.
pub fn key_from_url(href: &str, base_url: &str) -> Option<String> {
    let site = Url::parse(base_url).ok()?;
    let target = site.join(href).ok()?;
    if !matches!(target.scheme(), "http" | "https")
        || target.scheme() != site.scheme()
        || target.host_str() != site.host_str()
        || target.port_or_known_default() != site.port_or_known_default()
    {
        return None;
    }

    let context = site.path().trim_end_matches('/');
    let path = target.path();
    let browse = format!("{context}/browse/");
    let key = if let Some(key) = path.strip_prefix(&browse) {
        key
    } else {
        // Jira Cloud also links to issues from project views.
        let prefix = format!("{context}/jira/software/projects/");
        let suffix = path.strip_prefix(&prefix)?;
        let (_, issue) = suffix.split_once("/issues/")?;
        issue
    };
    let key = key.to_ascii_uppercase();
    valid_key(&key).then_some(key)
}

fn escaped(text: &str) -> String {
    glib::markup_escape_text(text).to_string()
}

fn link(href: &str, label: &str) -> String {
    format!("<a href=\"{}\"><b>{}</b></a>", escaped(href), escaped(label))
}

pub fn markup_from_text(text: &str, base_url: &str) -> String {
    let bytes = text.as_bytes();
    let mut result = String::new();
    let mut i = 0;
    let mut plain_from = 0;
    while i < bytes.len() {
        // Jira Server's wiki links: [description|https://jira/browse/ART-123].
        if bytes[i] == b'['
            && let Some(close) = text[i + 1..].find(']')
        {
            let end = i + close + 2;
            let inside = &text[i + 1..end - 1];
            if let Some((label, url)) = inside.split_once('|')
                && let Some(key) = key_from_url(url, base_url)
            {
                result.push_str(&escaped(&text[plain_from..i]));
                let title = if label == url || label.trim().is_empty() { &key } else { label };
                result.push_str(&link(url, title));
                i = end;
                plain_from = i;
                continue;
            }
        }

        // Parse full URL first, so an unrelated site's /browse/ART-123 does not
        // become a clickable local issue key by accident.
        if bytes[i..].starts_with(b"https://") || bytes[i..].starts_with(b"http://") {
            let mut end = i;
            while end < bytes.len()
                && !bytes[end].is_ascii_whitespace()
                && !matches!(bytes[end], b'<' | b'>' | b'"' | b'\'' | b'[' | b']' | b'(' | b')')
            {
                end += 1;
            }
            let mut trimmed = end;
            while trimmed > i
                && matches!(bytes[trimmed - 1], b'.' | b',' | b';' | b':' | b'!' | b'?')
            {
                trimmed -= 1;
            }
            if let Some(key) = key_from_url(&text[i..trimmed], base_url) {
                result.push_str(&escaped(&text[plain_from..i]));
                result.push_str(&link(&text[i..trimmed], &key));
                i = trimmed;
                plain_from = i;
            } else {
                i = end;
            }
            continue;
        }

        let boundary = i == 0 || !matches!(bytes[i - 1], b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-');
        if boundary && bytes[i].is_ascii_uppercase() {
            let mut dash = i;
            while dash < bytes.len()
                && (bytes[dash].is_ascii_uppercase() || bytes[dash].is_ascii_digit() || bytes[dash] == b'_')
            {
                dash += 1;
            }
            if dash < bytes.len() && bytes[dash] == b'-' {
                let mut end = dash + 1;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                let after = end == bytes.len()
                    || !matches!(bytes[end], b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-');
                if end > dash + 1 && after {
                    let key = &text[i..end];
                    result.push_str(&escaped(&text[plain_from..i]));
                    result.push_str(&link(&format!("{}/browse/{key}", base_url.trim_end_matches('/')), key));
                    i = end;
                    plain_from = i;
                    continue;
                }
            }
        }
        i += 1;
    }
    result.push_str(&escaped(&text[plain_from..]));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const SITE: &str = "https://jira.example.com/jira";

    #[test]
    fn detects_issue_links_only_on_the_connected_site() {
        assert_eq!(key_from_url("https://jira.example.com/jira/browse/ART-2272", SITE).as_deref(), Some("ART-2272"));
        assert_eq!(key_from_url("/jira/browse/AFB-12?focusedCommentId=20", SITE).as_deref(), Some("AFB-12"));
        assert_eq!(key_from_url("https://jira.example.com/jira/jira/software/projects/ART/issues/ART-44", SITE).as_deref(), Some("ART-44"));
        assert_eq!(key_from_url("https://other.example.com/jira/browse/ART-1", SITE), None);
        assert_eq!(key_from_url("https://jira.example.com/browse/ART-1", SITE), None);
        assert_eq!(key_from_url("https://jira.example.com/jira/browse/ART-1/extra", SITE), None);
        assert_eq!(key_from_url("javascript:alert(1)", SITE), None);
        assert_eq!(key_from_url("https://jira.example.com/jira/browse/not-an-issue", SITE), None);
    }

    #[test]
    fn links_keys_and_shortens_urls_without_corrupting_surrounding_text() {
        let markup = markup_from_text("See ART-2272, (AFB-42) and https://jira.example.com/jira/browse/ART-99.", SITE);
        assert!(markup.contains("<b>ART-2272</b></a>,"));
        assert!(markup.contains("(<a href="));
        assert!(markup.contains("<b>ART-99</b></a>."));
        assert!(!markup.contains(">https://jira.example.com/jira/browse/ART-99</a>"));
    }

    #[test]
    fn does_not_intercept_urls_to_other_sites_or_false_issue_keys() {
        let markup = markup_from_text("https://other.example.com/browse/ART-22 ABC-3-x ABC-34a and art-3", SITE);
        assert!(!markup.contains("<a href="));
    }

    #[test]
    fn supports_legacy_wiki_links_and_escapes_markup() {
        let markup = markup_from_text("[related issue|https://jira.example.com/jira/browse/ART-8] & <ART-4>", SITE);
        assert!(markup.contains("<b>related issue</b></a>"));
        assert!(markup.contains("&amp; &lt;"));
        assert!(markup.contains("<b>ART-4</b></a>&gt;"));
    }

    #[test]
    fn handles_unicode_and_newlines_around_keys() {
        let markup = markup_from_text("Pozri ART-3\nďalej AFB-9", SITE);
        assert!(markup.contains("<b>ART-3</b></a>\n"));
        assert!(markup.contains("<b>AFB-9</b></a>"));
    }
}
