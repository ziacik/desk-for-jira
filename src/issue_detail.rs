use std::collections::{HashMap, HashSet};

use adw::prelude::*;
use gtk::glib;
use serde_json::Value;

use crate::jira::{Attachment, Issue};

pub type ImageSlots = HashMap<String, Vec<gtk::Box>>;

pub fn render(
    container: &gtk::Box,
    issue: &Issue,
    base_url: &str,
    toast: &adw::ToastOverlay,
) -> ImageSlots {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    let mut slots = ImageSlots::new();
    let mut used_images = HashSet::new();

    let header = gtk::Box::new(gtk::Orientation::Vertical, 10);
    header.add_css_class("detail-hero");
    let eyebrow = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let kind_label = if issue.kind.is_empty() { "Ticket" } else { &issue.kind };
    let kind = gtk::Label::new(Some(kind_label));
    kind.add_css_class("detail-kind");
    kind.set_halign(gtk::Align::Start);
    kind.set_xalign(0.5);
    eyebrow.append(&kind);

    // Only the spacer expands. A badge must never stretch across the inspector.
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    eyebrow.append(&spacer);

    // Branch names are useful occasionally, so a small direct copy action is enough.
    let branch_name = suggested_branch_name(&issue.key, &issue.summary);
    let copy_branch = gtk::Button::new();
    // Draw the actual Git branching graph, rather than an ambiguous copy glyph.
    // Bundled in code so the icon works even when the system icon theme lacks one.
    let branch_icon = gtk::DrawingArea::new();
    branch_icon.set_content_width(14);
    branch_icon.set_content_height(14);
    branch_icon.set_draw_func(|area, cr, width, height| {
        let size = f64::from(width.min(height));
        let offset_x = (f64::from(width) - size) / 2.0;
        let offset_y = (f64::from(height) - size) / 2.0;
        cr.translate(offset_x, offset_y);
        cr.scale(size / 20.0, size / 20.0);
        let color = area.color();
        cr.set_source_rgba(
            f64::from(color.red()),
            f64::from(color.green()),
            f64::from(color.blue()),
            f64::from(color.alpha()),
        );
        cr.set_line_width(1.7);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        cr.set_line_join(gtk::cairo::LineJoin::Round);
        cr.move_to(5.0, 5.0);
        cr.line_to(5.0, 15.0);
        cr.move_to(5.0, 11.0);
        cr.curve_to(10.0, 11.0, 15.0, 11.0, 15.0, 5.0);
        cr.stroke().expect("Git branch icon stroke");
        for (x, y) in [(5.0, 4.0), (5.0, 16.0), (15.0, 4.0)] {
            cr.arc(x, y, 2.0, 0.0, std::f64::consts::TAU);
            cr.fill().expect("Git branch icon node");
        }
    });
    copy_branch.set_child(Some(&branch_icon));
    copy_branch.add_css_class("flat");
    copy_branch.add_css_class("detail-branch-action");
    copy_branch.set_tooltip_text(Some("Copy suggested Git branch name"));
    let toast = toast.clone();
    copy_branch.connect_clicked(move |_| {
        if let Some(display) = gtk::gdk::Display::default() {
            display.clipboard().set_text(&branch_name);
            toast.add_toast(adw::Toast::new("Branch name copied"));
        }
    });
    eyebrow.append(&copy_branch);
    header.append(&eyebrow);

    let summary = gtk::Label::new(Some(&issue.summary));
    summary.add_css_class("detail-title");
    summary.set_wrap(true);
    summary.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    summary.set_selectable(true);
    summary.set_xalign(0.0);
    header.append(&summary);
    if let Some(epic) = &issue.epic {
        let epic_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let epic_caption = gtk::Label::new(Some("Epic"));
        epic_caption.add_css_class("detail-field-label");
        epic_row.append(&epic_caption);
        let title = if epic.summary.is_empty() { &epic.key } else { &epic.summary };
        if base_url.is_empty() {
            epic_row.append(&gtk::Label::new(Some(title)));
        } else {
            let url = format!("{base_url}/browse/{}", epic.key);
            let link = gtk::LinkButton::with_label(&url, title);
            link.set_tooltip_text(Some(&epic.key));
            epic_row.append(&link);
        }
        header.append(&epic_row);
    }
    container.append(&header);

    let metadata = gtk::Grid::new();
    metadata.add_css_class("detail-metadata");
    metadata.set_row_spacing(12);
    metadata.set_column_spacing(16);
    for (index, (title, value)) in [
        ("Status", issue.status.as_str()),
        ("Priority", issue.priority.as_str()),
        ("Assignee", issue.assignee.as_str()),
        ("Updated", issue.updated.as_str()),
    ].into_iter().enumerate() {
        let cell = gtk::Box::new(gtk::Orientation::Vertical, 5);
        cell.set_hexpand(true);
        let caption = gtk::Label::new(Some(title));
        caption.add_css_class("detail-field-label");
        caption.set_xalign(0.0);
        cell.append(&caption);
        let value = gtk::Label::new(Some(if value.is_empty() { "—" } else { value }));
        value.add_css_class("detail-field-value");
        if title == "Status" {
            value.add_css_class("detail-status");
        }
        value.set_wrap(true);
        value.set_xalign(0.0);
        value.set_selectable(true);
        cell.append(&value);
        metadata.attach(&cell, (index % 2) as i32, (index / 2) as i32, 1, 1);
    }
    container.append(&metadata);

    let section = gtk::Box::new(gtk::Orientation::Vertical, 12);
    section.add_css_class("detail-section");
    let heading = gtk::Label::new(Some("Description"));
    heading.add_css_class("detail-section-title");
    heading.set_xalign(0.0);
    section.append(&heading);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 10);
    body.add_css_class("detail-body");
    let rendered = issue.description_doc.as_ref()
        .and_then(|doc| doc.get("content").and_then(Value::as_array))
        .filter(|nodes| !nodes.is_empty());
    if let Some(nodes) = rendered {
        for node in nodes {
            render_block(&body, node, &issue.attachments, &mut used_images, &mut slots);
        }
    } else if !issue.description.is_empty() {
        render_legacy(&body, &issue.description, &issue.attachments, &mut used_images, &mut slots);
    } else {
        add_text(&body, "No description yet.", "detail-empty");
    }
    section.append(&body);
    container.append(&section);

    let remaining: Vec<_> = issue.attachments.iter()
        .filter(|a| !used_images.contains(&a.id))
        .collect();
    if !remaining.is_empty() {
        let attachments = gtk::Box::new(gtk::Orientation::Vertical, 10);
        attachments.add_css_class("detail-section");
        let title = gtk::Label::new(Some("Attachments"));
        title.add_css_class("detail-section-title");
        title.set_xalign(0.0);
        attachments.append(&title);
        for attachment in remaining {
            if attachment.is_image() {
                append_image(&attachments, attachment, &mut slots);
            } else if safe_url(&attachment.content_url) {
                let link = gtk::LinkButton::with_label(&attachment.content_url, &attachment.filename);
                link.set_halign(gtk::Align::Start);
                attachments.append(&link);
            } else {
                add_text(&attachments, &attachment.filename, "issue-prose");
            }
        }
        container.append(&attachments);
    }

    slots
}

fn suggested_branch_name(key: &str, summary: &str) -> String {
    // Jira's default: ISSUE-123-summary-in-lowercase. Keep branch names
    // readable and safe for Git refs, including issues titled in Slovak.
    const MAX_LENGTH: usize = 100;
    let key = key.trim().to_ascii_uppercase();
    let max_slug_length = MAX_LENGTH.saturating_sub(key.len().saturating_add(1));
    if max_slug_length == 0 {
        return key;
    }

    let mut slug = String::new();
    for character in summary.chars().flat_map(char::to_lowercase) {
        let replacement = match character {
            'a'..='z' | '0'..='9' => {
                if slug.len() < max_slug_length {
                    slug.push(character);
                }
                if slug.len() >= max_slug_length {
                    break;
                }
                continue;
            }
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'ā' | 'ą' | 'ă' => "a",
            'č' | 'ć' | 'ç' | 'ĉ' | 'ċ' => "c",
            'ď' | 'đ' | 'ð' => "d",
            'é' | 'è' | 'ê' | 'ë' | 'ě' | 'ē' | 'ę' | 'ė' => "e",
            'í' | 'ì' | 'î' | 'ï' | 'ī' | 'ı' => "i",
            'ľ' | 'ĺ' | 'ł' => "l",
            'ň' | 'ń' | 'ñ' => "n",
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' | 'ō' | 'ő' => "o",
            'ŕ' | 'ř' => "r",
            'š' | 'ś' | 'ș' | 'ş' => "s",
            'ť' | 'ț' | 'ţ' => "t",
            'ú' | 'ù' | 'û' | 'ü' | 'ů' | 'ū' | 'ű' => "u",
            'ý' | 'ÿ' => "y",
            'ž' | 'ź' | 'ż' => "z",
            'æ' => "ae",
            'œ' => "oe",
            'ß' => "ss",
            'þ' => "th",
            _ => "-",
        };
        for ascii in replacement.chars() {
            if ascii == '-' {
                if !slug.is_empty() && !slug.ends_with('-') && slug.len() < max_slug_length {
                    slug.push('-');
                }
            } else if slug.len() < max_slug_length {
                slug.push(ascii);
            }
        }
        if slug.len() >= max_slug_length {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        key
    } else {
        format!("{key}-{slug}")
    }
}

pub fn set_image(slots: &ImageSlots, id: &str, bytes: Option<&[u8]>) {
    let Some(containers) = slots.get(id) else { return };
    let texture = bytes.and_then(|data| {
        gtk::gdk::Texture::from_bytes(&glib::Bytes::from(data)).ok()
    });
    for container in containers {
        while let Some(child) = container.first_child() {
            container.remove(&child);
        }
        if let Some(ref texture) = texture {
            let picture = gtk::Picture::for_paintable(texture);
            picture.set_content_fit(gtk::ContentFit::Contain);
            picture.set_can_shrink(true);
            picture.set_hexpand(true);
            picture.set_height_request(220);
            container.append(&picture);
        } else {
            add_text(container, "Preview unavailable — open in Jira to view.", "detail-empty");
        }
    }
}

fn add_text(container: &gtk::Box, text: &str, class: &str) {
    add_markup(container, &glib::markup_escape_text(text), class);
}

fn add_markup(container: &gtk::Box, markup: &str, class: &str) {
    let label = gtk::Label::new(None);
    label.set_markup(markup);
    label.add_css_class(class);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_selectable(true);
    label.set_xalign(0.0);
    label.set_valign(gtk::Align::Start);
    container.append(&label);
}

fn render_block(
    parent: &gtk::Box,
    node: &Value,
    attachments: &[Attachment],
    used: &mut HashSet<String>,
    slots: &mut ImageSlots,
) {
    let kind = node.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "paragraph" => {
            let markup = inline_children(node);
            if !markup.is_empty() {
                add_markup(parent, &markup, "issue-prose");
            }
        }
        "heading" => {
            let level = node.get("attrs").and_then(|a| a.get("level"))
                .and_then(Value::as_u64).unwrap_or(2);
            let class = if level <= 2 { "issue-heading" } else { "issue-subheading" };
            add_markup(parent, &inline_children(node), class);
        }
        "bulletList" | "orderedList" => {
            let start = node.get("attrs").and_then(|a| a.get("order"))
                .and_then(Value::as_u64).unwrap_or(1);
            for (index, item) in children(node).iter().enumerate() {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 9);
                row.add_css_class("issue-list-row");
                let prefix = if kind == "bulletList" {
                    "•".to_owned()
                } else {
                    format!("{}.", start + index as u64)
                };
                let bullet = gtk::Label::new(Some(&prefix));
                bullet.add_css_class("issue-list-marker");
                bullet.set_valign(gtk::Align::Start);
                row.append(&bullet);
                let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
                content.set_hexpand(true);
                for child in children(item) {
                    render_block(&content, child, attachments, used, slots);
                }
                row.append(&content);
                parent.append(&row);
            }
        }
        "blockquote" | "panel" => {
            let box_ = gtk::Box::new(gtk::Orientation::Vertical, 8);
            box_.add_css_class(if kind == "panel" { "issue-panel" } else { "issue-quote" });
            for child in children(node) {
                render_block(&box_, child, attachments, used, slots);
            }
            parent.append(&box_);
        }
        "codeBlock" => {
            let mut code = String::new();
            collect_text(node, &mut code);
            add_text(parent, code.trim_end_matches('\n'), "issue-code");
        }
        "rule" => parent.append(&gtk::Separator::new(gtk::Orientation::Horizontal)),
        "mediaSingle" | "mediaGroup" => {
            for child in children(node) {
                render_block(parent, child, attachments, used, slots);
            }
        }
        "media" => {
            if let Some(attachment) = match_media(node, attachments, used) {
                used.insert(attachment.id.clone());
                append_image(parent, attachment, slots);
            } else {
                add_text(parent, "Image available in Jira", "detail-empty");
            }
        }
        "table" => {
            let table = gtk::Box::new(gtk::Orientation::Vertical, 0);
            table.add_css_class("issue-table");
            for row in children(node) {
                let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                row_box.add_css_class("issue-table-row");
                for cell in children(row) {
                    let cell_box = gtk::Box::new(gtk::Orientation::Vertical, 5);
                    cell_box.add_css_class("issue-table-cell");
                    cell_box.set_hexpand(true);
                    for child in children(cell) {
                        render_block(&cell_box, child, attachments, used, slots);
                    }
                    row_box.append(&cell_box);
                }
                table.append(&row_box);
            }
            parent.append(&table);
        }
        _ => {
            for child in children(node) {
                render_block(parent, child, attachments, used, slots);
            }
        }
    }
}

fn children(node: &Value) -> &[Value] {
    node.get("content").and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn inline_children(node: &Value) -> String {
    children(node).iter().map(inline_markup).collect()
}

fn inline_markup(node: &Value) -> String {
    let kind = node.get("type").and_then(Value::as_str).unwrap_or("");
    let text = match kind {
        "text" => node.get("text").and_then(Value::as_str).unwrap_or(""),
        "hardBreak" => return "\n".into(),
        "mention" => node.get("attrs").and_then(|a| a.get("text"))
            .and_then(Value::as_str).unwrap_or(""),
        "emoji" => node.get("attrs").and_then(|a| a.get("text"))
            .and_then(Value::as_str).unwrap_or(""),
        "inlineCard" => {
            let url = node.get("attrs").and_then(|a| a.get("url"))
                .and_then(Value::as_str).unwrap_or("");
            if safe_url(url) {
                return format!("<a href=\"{}\">{}</a>",
                    glib::markup_escape_text(url), glib::markup_escape_text(url));
            }
            return glib::markup_escape_text(url).to_string();
        }
        _ => return inline_children(node),
    };
    let mut markup = glib::markup_escape_text(text).to_string();
    if let Some(marks) = node.get("marks").and_then(Value::as_array) {
        for mark in marks {
            match mark.get("type").and_then(Value::as_str).unwrap_or("") {
                "strong" => markup = format!("<b>{markup}</b>"),
                "em" => markup = format!("<i>{markup}</i>"),
                "strike" => markup = format!("<s>{markup}</s>"),
                "underline" => markup = format!("<u>{markup}</u>"),
                "code" => markup = format!("<tt>{markup}</tt>"),
                "link" => {
                    if let Some(url) = mark.get("attrs").and_then(|a| a.get("href"))
                        .and_then(Value::as_str).filter(|url| safe_url(url))
                    {
                        markup = format!("<a href=\"{}\">{markup}</a>",
                            glib::markup_escape_text(url));
                    }
                }
                _ => {}
            }
        }
    }
    markup
}

fn collect_text(node: &Value, out: &mut String) {
    if let Some(text) = node.get("text").and_then(Value::as_str) {
        out.push_str(text);
    }
    if node.get("type").and_then(Value::as_str) == Some("hardBreak") {
        out.push('\n');
    }
    for child in children(node) {
        collect_text(child, out);
    }
    if node.get("type").and_then(Value::as_str) == Some("paragraph") {
        out.push('\n');
    }
}

fn match_media<'a>(
    node: &Value,
    attachments: &'a [Attachment],
    used: &HashSet<String>,
) -> Option<&'a Attachment> {
    let attrs = node.get("attrs")?;
    let id = attrs.get("id").and_then(Value::as_str);
    let alt = attrs.get("alt").and_then(Value::as_str);
    attachments.iter().find(|a| a.is_image() && !used.contains(&a.id)
        && id == Some(a.id.as_str()))
        .or_else(|| attachments.iter().find(|a| a.is_image() && !used.contains(&a.id)
            && alt.is_some_and(|alt| alt.eq_ignore_ascii_case(&a.filename))))
        // ADF media UUIDs differ from REST attachment IDs. Jira gives us no
        // reliable ID mapping, so fall back to image attachment order.
        .or_else(|| attachments.iter().find(|a| a.is_image() && !used.contains(&a.id)))
}

fn append_image(parent: &gtk::Box, attachment: &Attachment, slots: &mut ImageSlots) {
    let block = gtk::Box::new(gtk::Orientation::Vertical, 5);
    block.add_css_class("issue-image-card");
    let slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
    slot.add_css_class("issue-image-slot");
    if attachment.content_url.is_empty() {
        add_text(&slot, "Preview unavailable — open in Jira to view.", "detail-empty");
    } else {
        add_text(&slot, "Loading image…", "detail-empty");
        slots.entry(attachment.id.clone()).or_default().push(slot.clone());
    }
    block.append(&slot);
    let caption = gtk::Label::new(Some(&attachment.filename));
    caption.add_css_class("issue-image-caption");
    caption.set_xalign(0.0);
    caption.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    block.append(&caption);
    parent.append(&block);
}

fn render_legacy(
    parent: &gtk::Box,
    text: &str,
    attachments: &[Attachment],
    used: &mut HashSet<String>,
    slots: &mut ImageSlots,
) {
    for paragraph in text.split("\n\n").filter(|p| !p.trim().is_empty()) {
        let trimmed = paragraph.trim();
        let filename = trimmed.strip_prefix('!').and_then(|s| s.strip_suffix('!'))
            .and_then(|s| s.split('|').next());
        if let Some(attachment) = filename.and_then(|name| attachments.iter()
            .find(|a| a.is_image() && a.filename.eq_ignore_ascii_case(name)))
        {
            used.insert(attachment.id.clone());
            append_image(parent, attachment, slots);
        } else if let Some(heading) = trimmed.strip_prefix("h2. ")
            .or_else(|| trimmed.strip_prefix("h3. "))
        {
            add_text(parent, heading, "issue-heading");
        } else {
            add_text(parent, paragraph, "issue-prose");
        }
    }
}

fn safe_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn attachment(id: &str, filename: &str) -> Attachment {
        Attachment {
            id: id.into(),
            filename: filename.into(),
            mime_type: "image/png".into(),
            content_url: format!("https://jira.example.com/attachment/{id}"),
        }
    }

    #[test]
    fn branch_name_uses_jira_key_and_summary() {
        assert_eq!(
            suggested_branch_name("art-123", "Fix issue detail layout"),
            "ART-123-fix-issue-detail-layout"
        );
        assert_eq!(
            suggested_branch_name("AFB-42", "Vylepšiť zobrazenie záložiek"),
            "AFB-42-vylepsit-zobrazenie-zaloziek"
        );
        assert_eq!(
            suggested_branch_name("ART-5", "  🐛 Fix API / OAuth & SSO!  "),
            "ART-5-fix-api-oauth-sso"
        );
        assert_eq!(suggested_branch_name("ART-6", "!!!"), "ART-6");
    }

    #[test]
    fn branch_name_is_bounded_and_has_no_trailing_separator() {
        let name = suggested_branch_name("ART-42", &format!("{}!", "Long summary ".repeat(30)));
        assert!(name.len() <= 100);
        assert!(name.starts_with("ART-42-"));
        assert!(!name.ends_with('-'));
        assert!(name.is_ascii());
    }

    #[test]
    fn adf_inline_marks_and_links_are_escaped() {
        let node = json!({"type":"paragraph","content":[
            {"type":"text","text":"A < B & C", "marks":[{"type":"strong"}]},
            {"type":"text","text":" click", "marks":[{"type":"link","attrs":{"href":"https://example.com/?a=1&b=2"}}]},
            {"type":"text","text":" unsafe", "marks":[{"type":"link","attrs":{"href":"javascript:alert(1)"}}]}
        ]});
        let markup = inline_children(&node);
        assert!(markup.contains("<b>A &lt; B &amp; C</b>"));
        assert!(markup.contains("<a href=\"https://example.com/?a=1&amp;b=2\"> click</a>"));
        assert!(!markup.contains("javascript:"));
    }

    #[test]
    fn adf_media_uses_filename_before_order_fallback() {
        let attachments = vec![attachment("1", "first.png"), attachment("2", "target.png")];
        let node = json!({"type":"media","attrs":{"id":"not-an-attachment-id","alt":"target.png"}});
        let used = HashSet::new();
        assert_eq!(match_media(&node, &attachments, &used).unwrap().id, "2");
    }
}
