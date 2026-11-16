//! Turn trusted Markdown parser events into safe, native GTK/Pango content.
//! Never interpret raw Markdown HTML as GTK markup.
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Paragraph(String),
    Heading(String),
    Quote(String),
    Code(String),
    Rule,
}

fn escaped(text: &str) -> String {
    gtk::glib::markup_escape_text(text).to_string()
}

fn safe_href(href: &str) -> bool {
    let lower = href.to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://")
        || lower.starts_with("mailto:")
}

fn finish(blocks: &mut Vec<Block>, text: &mut String, quote: bool, heading: bool) {
    if !text.trim().is_empty() {
        let result = std::mem::take(text);
        blocks.push(if heading {
            Block::Heading(result)
        } else if quote {
            Block::Quote(result)
        } else {
            Block::Paragraph(result)
        });
    } else {
        text.clear();
    }
}

/// The same parser is used for completed messages and partial streamed replies.
/// Text and HTML originating with Codex are escaped before GTK receives them.
pub fn blocks(markdown: &str) -> Vec<Block> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS);
    let mut result = Vec::new();
    let mut text = String::new();
    let mut code = false;
    let mut heading = false;
    let mut quote_depth = 0_usize;
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut links: Vec<bool> = Vec::new();

    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { .. } => {
                    finish(&mut result, &mut text, quote_depth > 0, heading);
                    heading = true;
                }
                Tag::CodeBlock(_) => {
                    finish(&mut result, &mut text, quote_depth > 0, heading);
                    code = true;
                }
                Tag::List(start) => lists.push(start),
                Tag::Item => {
                    finish(&mut result, &mut text, quote_depth > 0, heading);
                    let depth = lists.len().saturating_sub(1);
                    text.push_str(&"  ".repeat(depth));
                    let prefix = if let Some(Some(next)) = lists.last_mut() {
                        let current = *next;
                        *next = next.saturating_add(1);
                        format!("{current}. ")
                    } else {
                        "• ".to_owned()
                    };
                    text.push_str(&prefix);
                }
                Tag::BlockQuote(_) => {
                    finish(&mut result, &mut text, quote_depth > 0, heading);
                    quote_depth += 1;
                }
                Tag::Emphasis => text.push_str("<i>"),
                Tag::Strong => text.push_str("<b>"),
                Tag::Strikethrough => text.push_str("<s>"),
                Tag::Link { dest_url, .. } => {
                    let valid = safe_href(&dest_url);
                    links.push(valid);
                    if valid {
                        text.push_str(&format!("<a href=\"{}\">", escaped(&dest_url)));
                    }
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    finish(&mut result, &mut text, quote_depth > 0, heading);
                    heading = false;
                }
                TagEnd::Paragraph => finish(&mut result, &mut text, quote_depth > 0, heading),
                TagEnd::CodeBlock => {
                    code = false;
                    if !text.is_empty() {
                        result.push(Block::Code(std::mem::take(&mut text)));
                    }
                }
                TagEnd::Item => finish(&mut result, &mut text, quote_depth > 0, heading),
                TagEnd::List(_) => {
                    finish(&mut result, &mut text, quote_depth > 0, heading);
                    lists.pop();
                }
                TagEnd::BlockQuote(_) => {
                    finish(&mut result, &mut text, quote_depth > 0, heading);
                    quote_depth = quote_depth.saturating_sub(1);
                }
                TagEnd::Emphasis => text.push_str("</i>"),
                TagEnd::Strong => text.push_str("</b>"),
                TagEnd::Strikethrough => text.push_str("</s>"),
                TagEnd::Link => {
                    if links.pop() == Some(true) {
                        text.push_str("</a>");
                    }
                }
                _ => {}
            },
            Event::Text(value) => {
                if code { text.push_str(&value); } else { text.push_str(&escaped(&value)); }
            }
            Event::Code(value) => {
                text.push_str("<tt>");
                text.push_str(&escaped(&value));
                text.push_str("</tt>");
            }
            Event::SoftBreak => text.push(' '),
            Event::HardBreak => text.push('\n'),
            Event::Rule => {
                finish(&mut result, &mut text, quote_depth > 0, heading);
                result.push(Block::Rule);
            }
            Event::TaskListMarker(checked) => text.push_str(if checked { "☑ " } else { "☐ " }),
            Event::Html(value) | Event::InlineHtml(value) => text.push_str(&escaped(&value)),
            Event::InlineMath(value) | Event::DisplayMath(value) => {
                text.push_str(&escaped(&value));
            }
            _ => {}
        }
    }
    if !text.is_empty() {
        if code {
            result.push(Block::Code(text));
        } else {
            // A partially received tag may be unbalanced while streaming.
            // The Markdown parser normally emits balanced events even for partial input.
            finish(&mut result, &mut text, quote_depth > 0, heading);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_the_issue_reply_without_literal_markdown() {
        let result = blocks("The fix is **implemented** and `npm test` passed.\n\n- one\n- two");
        assert_eq!(result[0], Block::Paragraph(
            "The fix is <b>implemented</b> and <tt>npm test</tt> passed.".into()
        ));
        assert_eq!(result[1], Block::Paragraph("• one".into()));
        assert_eq!(result[2], Block::Paragraph("• two".into()));
    }

    #[test]
    fn fences_and_headings_have_distinct_blocks() {
        let rendered = blocks("## Test\n\n```rust\nlet a = 1 < 2;\n```");
        assert_eq!(rendered[0], Block::Heading("Test".into()));
        assert_eq!(rendered[1], Block::Code("let a = 1 < 2;\n".into()));
    }

    #[test]
    fn escapes_markup_and_allows_only_safe_links() {
        let rendered = blocks("[safe](https://example.org/?a=1&b=2) [bad](javascript:alert(1)) <script>alert(1)</script>");
        let content = format!("{rendered:?}");
        assert!(content.contains("href=\\\"https://example.org/?a=1&amp;b=2\\\""));
        assert!(!content.contains("href=\\\"javascript:"));
        assert!(!content.contains("<script>"));
    }

    #[test]
    fn works_with_partial_streamed_markdown() {
        let partial = blocks("**already implement");
        let full = blocks("**already implemented**");
        assert!(format!("{partial:?}").contains("already implement"));
        assert_eq!(full[0], Block::Paragraph("<b>already implemented</b>".into()));
    }
}
