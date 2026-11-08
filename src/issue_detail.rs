use std::{collections::{HashMap, HashSet}, rc::Rc};

use adw::prelude::*;
use gtk::glib;
use serde_json::Value;

use crate::{issue_links, jira::{Attachment, Issue, IssueComment}};

pub type ImageSlots = HashMap<String, Vec<(gtk::Box, String)>>;
pub type OpenIssueLink = Rc<dyn Fn(&str)>;

pub struct DetailRender {
    pub images: ImageSlots,
    pub comments: gtk::Box,
    pub children: gtk::Box,
}

/// Both header actions use exactly the same symbolic drawing system:
/// size, stroke, color and alignment follow the current GTK theme.
#[derive(Clone, Copy)]
pub(crate) enum HeaderActionIcon {
    Branch,
    ExternalLink,
}

pub(crate) fn header_action_icon(kind: HeaderActionIcon) -> gtk::DrawingArea {
    let icon = gtk::DrawingArea::new();
    icon.set_content_width(16);
    icon.set_content_height(16);
    icon.set_halign(gtk::Align::Center);
    icon.set_valign(gtk::Align::Center);
    icon.set_draw_func(move |area, cr, width, height| {
        // DrawingArea's content size is a minimum, not a maximum: GtkButton
        // can allocate much more than 16px. Keep the glyph fixed-size even
        // if the widget is ever expanded by its parent.
        let size = f64::from(width.min(height)).min(16.0);
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
        cr.set_line_width(1.8);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        cr.set_line_join(gtk::cairo::LineJoin::Round);

        match kind {
            HeaderActionIcon::Branch => {
                cr.move_to(5.0, 5.0);
                cr.line_to(5.0, 15.0);
                cr.move_to(5.0, 11.0);
                cr.curve_to(10.0, 11.0, 15.0, 11.0, 15.0, 5.0);
                cr.stroke().expect("Branch icon stroke");
                for (x, y) in [(5.0, 4.0), (5.0, 16.0), (15.0, 4.0)] {
                    cr.arc(x, y, 1.8, 0.0, std::f64::consts::TAU);
                    cr.fill().expect("Branch icon node");
                }
            }
            HeaderActionIcon::ExternalLink => {
                // Conventional external-link glyph: a square with an arrow
                // pointing out through its open upper-right corner.
                cr.move_to(8.0, 5.0);
                cr.line_to(5.5, 5.0);
                cr.curve_to(4.7, 5.0, 4.0, 5.7, 4.0, 6.5);
                cr.line_to(4.0, 14.5);
                cr.curve_to(4.0, 15.3, 4.7, 16.0, 5.5, 16.0);
                cr.line_to(13.5, 16.0);
                cr.curve_to(14.3, 16.0, 15.0, 15.3, 15.0, 14.5);
                cr.line_to(15.0, 12.0);
                cr.move_to(10.0, 4.0);
                cr.line_to(16.0, 4.0);
                cr.line_to(16.0, 10.0);
                cr.move_to(16.0, 4.0);
                cr.line_to(8.0, 12.0);
                cr.stroke().expect("External link icon stroke");
            }
        }
    });
    icon
}

fn section_heading(title: &str, icon: &str, count: Option<usize>) -> gtk::Box {
    let heading = gtk::Box::new(gtk::Orientation::Horizontal, 9);
    heading.add_css_class("detail-section-heading");
    let image = gtk::Image::from_icon_name(icon);
    image.add_css_class("detail-section-icon");
    heading.append(&image);
    let label = gtk::Label::new(Some(title));
    label.add_css_class("detail-section-title");
    label.set_xalign(0.0);
    heading.append(&label);
    if let Some(count) = count {
        let badge = gtk::Label::new(Some(&count.to_string()));
        badge.add_css_class("detail-section-count");
        heading.append(&badge);
    }
    heading
}

fn issue_relation_row(
    relation: &str,
    key: &str,
    summary: &str,
    on_issue_link: &OpenIssueLink,
) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 9);
    row.add_css_class("detail-relation-row");
    let icon = gtk::Image::from_icon_name("view-list-symbolic");
    icon.add_css_class("detail-relation-icon");
    row.append(&icon);
    let caption = gtk::Label::new(Some(relation));
    caption.add_css_class("detail-relation-caption");
    row.append(&caption);

    let display = if summary.is_empty() {
        key.to_owned()
    } else {
        format!("{key} · {summary}")
    };
    let button = gtk::Button::new();
    button.add_css_class("detail-relation-link");
    button.set_halign(gtk::Align::Start);
    button.set_hexpand(true);
    button.set_tooltip_text(Some(&format!("Open {key}")));
    let text = gtk::Label::new(Some(&display));
    text.set_wrap(true);
    text.set_xalign(0.0);
    button.set_child(Some(&text));
    let key = key.to_owned();
    let on_issue_link = on_issue_link.clone();
    button.connect_clicked(move |_| on_issue_link(&key));
    row.append(&button);
    row
}

pub fn render_children(
    container: &gtk::Box,
    children: &[Issue],
    on_issue_link: &OpenIssueLink,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
    if children.is_empty() {
        container.set_visible(false);
        return;
    }

    container.append(&section_heading(
        "Child tickets", "view-list-symbolic", Some(children.len()),
    ));
    let list = gtk::Box::new(gtk::Orientation::Vertical, 6);
    list.add_css_class("detail-children-list");
    for issue in children {
        let button = gtk::Button::new();
        button.add_css_class("detail-child-link");
        button.set_tooltip_text(Some(&format!("Open {}", issue.key)));
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let title = gtk::Box::new(gtk::Orientation::Vertical, 3);
        title.set_hexpand(true);
        let key = gtk::Label::new(Some(&issue.key));
        key.add_css_class("detail-child-key");
        key.set_xalign(0.0);
        title.append(&key);
        let summary = gtk::Label::new(Some(&issue.summary));
        summary.add_css_class("detail-child-summary");
        summary.set_wrap(true);
        summary.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        summary.set_xalign(0.0);
        title.append(&summary);
        content.append(&title);
        let status = gtk::Label::new(Some(&issue.status));
        status.add_css_class("detail-child-status");
        status.set_valign(gtk::Align::Center);
        content.append(&status);
        button.set_child(Some(&content));
        let key = issue.key.clone();
        let on_issue_link = on_issue_link.clone();
        button.connect_clicked(move |_| on_issue_link(&key));
        list.append(&button);
    }
    container.append(&list);
    container.set_visible(true);
}

pub fn show_children_error(container: &gtk::Box, error: &str) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
    container.append(&section_heading("Child tickets", "view-list-symbolic", None));
    add_text(container, &format!("Could not load child tickets: {error}"), "detail-empty");
    container.set_visible(true);
}

pub fn render(
    container: &gtk::Box,
    issue: &Issue,
    base_url: &str,
    actions: &gtk::Box,
    on_issue_link: &OpenIssueLink,
) -> DetailRender {
    // Keep the existing transition widget and its handlers alive when the
    // issue is refreshed, then reparent it into the new issue hero.
    if let Some(parent) = actions.parent().and_then(|widget| widget.downcast::<gtk::Box>().ok()) {
        parent.remove(actions);
    }
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    let mut slots = ImageSlots::new();
    let mut used_images = HashSet::new();

    let header = gtk::Box::new(gtk::Orientation::Vertical, 10);
    header.add_css_class("detail-hero");

    let eyebrow = gtk::Box::new(gtk::Orientation::Horizontal, 9);
    eyebrow.add_css_class("detail-eyebrow");
    let kind = issue.kind.to_ascii_lowercase();
    let (kind_icon, kind_class) = match kind.as_str() {
        "bug" => ("dialog-warning-symbolic", "kind-bug"),
        "story" => ("starred-symbolic", "kind-story"),
        _ => ("view-list-symbolic", "kind-task"),
    };
    let type_mark = gtk::CenterBox::new();
    type_mark.add_css_class("detail-type-mark");
    type_mark.add_css_class(kind_class);
    type_mark.set_size_request(27, 27);
    let type_icon = gtk::Image::from_icon_name(kind_icon);
    type_mark.set_center_widget(Some(&type_icon));
    eyebrow.append(&type_mark);
    let key = gtk::Label::new(Some(&issue.key));
    key.add_css_class("detail-key");
    key.set_selectable(true);
    eyebrow.append(&key);
    let kind_label = if issue.kind.is_empty() { "Ticket" } else { &issue.kind };
    let kind_badge = gtk::Label::new(Some(kind_label));
    kind_badge.add_css_class("detail-kind");
    eyebrow.append(&kind_badge);
    header.append(&eyebrow);

    let summary = gtk::Label::new(Some(&issue.summary));
    summary.add_css_class("detail-title");
    summary.set_wrap(true);
    summary.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    summary.set_selectable(true);
    summary.set_xalign(0.0);
    header.append(&summary);

    let badges = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    badges.add_css_class("detail-badges");
    let status = gtk::Label::new(Some(if issue.status.is_empty() { "—" } else { &issue.status }));
    status.add_css_class("detail-status-pill");
    badges.append(&status);
    let priority = gtk::Label::new(Some(if issue.priority.is_empty() { "—" } else { &issue.priority }));
    priority.add_css_class("detail-priority-pill");
    badges.append(&priority);
    header.append(&badges);

    if let Some(parent) = &issue.parent {
        let epic = issue.epic.as_ref().filter(|epic| epic.key == parent.key);
        let summary = if parent.summary.is_empty() {
            epic.map(|epic| epic.summary.as_str()).unwrap_or("")
        } else {
            &parent.summary
        };
        header.append(&issue_relation_row(
            if epic.is_some() { "Epic" } else { "Parent" },
            &parent.key, summary, on_issue_link,
        ));
    } else if let Some(epic) = &issue.epic {
        // Older Jira Server projects expose the Epic Link custom field instead.
        header.append(&issue_relation_row(
            "Epic", &epic.key, &epic.summary, on_issue_link,
        ));
    }

    // Workflow actions belong to the issue header, not to a detached strip.
    header.append(actions);
    container.append(&header);

    // Let properties occupy the available width rather than leaving two huge,
    // half-empty columns in wide inspectors. FlowBox automatically wraps at
    // narrower panel widths without hiding any Jira fields.
    let metadata = gtk::FlowBox::new();
    metadata.add_css_class("detail-metadata");
    metadata.set_selection_mode(gtk::SelectionMode::None);
    metadata.set_homogeneous(true);
    metadata.set_halign(gtk::Align::Fill);
    metadata.set_hexpand(true);
    metadata.set_min_children_per_line(1);
    metadata.set_max_children_per_line(4);
    metadata.set_row_spacing(15);
    metadata.set_column_spacing(16);
    let fix_versions = issue.fix_versions.join(", ");
    let affects_versions = issue.affects_versions.join(", ");
    for (title, value) in [
        ("Assignee", issue.assignee.as_str()),
        ("Updated", issue.updated.as_str()),
        ("Fix Version/s", fix_versions.as_str()),
        ("Affects Version/s", affects_versions.as_str()),
    ] {
        let cell = gtk::Box::new(gtk::Orientation::Vertical, 5);
        cell.set_size_request(148, -1);
        cell.set_hexpand(true);
        let caption = gtk::Label::new(Some(title));
        caption.add_css_class("detail-field-label");
        caption.set_xalign(0.0);
        cell.append(&caption);
        let value = gtk::Label::new(Some(if value.is_empty() { "—" } else { value }));
        value.add_css_class("detail-field-value");
        value.set_wrap(true);
        value.set_xalign(0.0);
        value.set_selectable(true);
        cell.append(&value);
        metadata.insert(&cell, -1);
    }
    container.append(&metadata);

    let children = gtk::Box::new(gtk::Orientation::Vertical, 9);
    children.add_css_class("detail-section");
    children.set_visible(false);
    container.append(&children);

    let section = gtk::Box::new(gtk::Orientation::Vertical, 12);
    section.add_css_class("detail-section");
    section.append(&section_heading("Description", "text-x-generic-symbolic", None));

    let body = gtk::Box::new(gtk::Orientation::Vertical, 10);
    body.add_css_class("detail-body");
    let rendered = issue.description_doc.as_ref()
        .and_then(|doc| doc.get("content").and_then(Value::as_array))
        .filter(|nodes| !nodes.is_empty());
    if let Some(nodes) = rendered {
        for node in nodes {
            render_block(&body, node, &issue.attachments, &mut used_images, &mut slots, base_url, on_issue_link);
        }
    } else if !issue.description.is_empty() {
        render_legacy(&body, &issue.description, &issue.attachments, &mut used_images, &mut slots, base_url, on_issue_link);
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
        attachments.append(&section_heading("Attachments", "mail-attachment-symbolic", Some(remaining.len())));
        for attachment in remaining {
            if attachment.is_image() {
                append_image(&attachments, attachment, &mut slots);
            } else {
                let file = gtk::Box::new(gtk::Orientation::Horizontal, 9);
                file.add_css_class("detail-file");
                let icon = gtk::Image::from_icon_name("text-x-generic-symbolic");
                icon.add_css_class("detail-file-icon");
                file.append(&icon);
                if safe_url(&attachment.content_url) {
                    let link = gtk::LinkButton::with_label(&attachment.content_url, &attachment.filename);
                    link.add_css_class("detail-file-link");
                    link.set_halign(gtk::Align::Start);
                    file.append(&link);
                } else {
                    let label = gtk::Label::new(Some(&attachment.filename));
                    label.set_xalign(0.0);
                    label.set_wrap(true);
                    file.append(&label);
                }
                attachments.append(&file);
            }
        }
        container.append(&attachments);
    }

    let comments_section = gtk::Box::new(gtk::Orientation::Vertical, 12);
    comments_section.add_css_class("detail-section");
    comments_section.append(&section_heading("Comments", "document-edit-symbolic", None));
    let comments = gtk::Box::new(gtk::Orientation::Vertical, 0);
    show_comments_loading(&comments);
    comments_section.append(&comments);
    container.append(&comments_section);

    DetailRender { images: slots, comments, children }
}

pub fn show_comments_loading(container: &gtk::Box) {
    clear_comments(container);
    add_text(container, "Loading comments…", "detail-empty");
}

pub fn show_comments_error(container: &gtk::Box, error: &str) {
    clear_comments(container);
    add_text(container, &format!("Could not load comments: {error}"), "detail-empty");
}

pub fn render_comments(
    container: &gtk::Box,
    comments: &[IssueComment],
    attachments: &[Attachment],
    base_url: &str,
    on_issue_link: &OpenIssueLink,
) -> ImageSlots {
    clear_comments(container);
    let mut slots = ImageSlots::new();
    if comments.is_empty() {
        add_text(container, "No comments yet.", "detail-empty");
        return slots;
    }
    let mut used_images = HashSet::new();
    for comment in comments {
        let card = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        card.add_css_class("issue-comment");

        let initial = comment.author.chars()
            .find(|c| c.is_alphanumeric())
            .map(|c| c.to_uppercase().collect::<String>())
            .unwrap_or_else(|| "?".to_owned());
        let avatar = gtk::Label::new(Some(&initial));
        avatar.add_css_class("issue-comment-avatar");
        avatar.set_size_request(34, 34);
        avatar.set_halign(gtk::Align::Start);
        avatar.set_valign(gtk::Align::Start);
        card.append(&avatar);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.add_css_class("issue-comment-content");
        content.set_hexpand(true);
        let heading = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let author = gtk::Label::new(Some(&comment.author));
        author.add_css_class("issue-comment-author");
        author.set_xalign(0.0);
        author.set_hexpand(true);
        author.set_ellipsize(gtk::pango::EllipsizeMode::End);
        heading.append(&author);
        let date = gtk::Label::new(Some(&format_comment_time(&comment.created)));
        date.add_css_class("issue-comment-date");
        date.set_xalign(1.0);
        heading.append(&date);
        content.append(&heading);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
        body.add_css_class("detail-body");
        if let Some(nodes) = comment.body.get("content").and_then(Value::as_array) {
            for node in nodes {
                render_block(&body, node, attachments, &mut used_images, &mut slots, base_url, on_issue_link);
            }
        } else if let Some(text) = comment.body.as_str() {
            render_legacy(&body, text, attachments, &mut used_images, &mut slots, base_url, on_issue_link);
        }
        if body.first_child().is_none() {
            add_text(&body, "Empty comment", "detail-empty");
        }
        content.append(&body);
        card.append(&content);
        container.append(&card);
    }
    slots
}

fn clear_comments(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn format_comment_time(created: &str) -> String {
    if let Some((day, rest)) = created.split_once('T')
        && let Some(time) = rest.get(..5)
    {
        return format!("{day} · {time}");
    }
    created.to_owned()
}

pub(crate) fn suggested_branch_name(key: &str, summary: &str) -> String {
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
    for (container, filename) in containers {
        while let Some(child) = container.first_child() {
            container.remove(&child);
        }
        if let Some(ref texture) = texture {
            let picture = gtk::Picture::for_paintable(texture);
            picture.set_content_fit(gtk::ContentFit::Contain);
            picture.set_can_shrink(true);
            picture.set_hexpand(true);
            picture.set_height_request(190);

            // Keep the preview keyboard-accessible and reuse the already loaded
            // texture for the full-resolution viewer (no second network request).
            let button = gtk::Button::new();
            button.add_css_class("issue-image-preview");
            button.set_hexpand(true);
            button.set_cursor_from_name(Some("zoom-in"));
            button.set_tooltip_text(Some("View full-size image"));
            button.set_child(Some(&picture));
            let texture = texture.clone();
            let filename = filename.clone();
            button.connect_clicked(move |button| {
                show_full_size_image(button, &texture, &filename);
            });
            container.append(&button);
        } else {
            add_text(container, "Preview unavailable — open in Jira to view.", "detail-empty");
        }
    }
}

fn show_full_size_image(button: &gtk::Button, texture: &gtk::gdk::Texture, filename: &str) {
    let Some(parent) = button.root().and_then(|root| root.downcast::<gtk::Window>().ok())
    else {
        return;
    };

    let viewer = gtk::Window::builder()
        .title(filename)
        .transient_for(&parent)
        .modal(true)
        .default_width(960)
        .default_height(700)
        .build();

    let header = gtk::HeaderBar::new();
    header.set_show_title_buttons(true);
    let dimensions = gtk::Label::new(Some(&format!("{} × {} px · 100%", texture.width(), texture.height())));
    dimensions.add_css_class("dim-label");
    header.pack_start(&dimensions);
    viewer.set_titlebar(Some(&header));

    // Never shrink the original in this viewer; scrolling exposes every pixel.
    // ScrolledWindow caps the viewport instead of letting giant images expand
    // the dialog beyond the screen.
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_hexpand(true);
    scroller.set_vexpand(true);
    scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    let picture = gtk::Picture::for_paintable(texture);
    picture.set_can_shrink(false);
    picture.set_halign(gtk::Align::Center);
    picture.set_valign(gtk::Align::Center);
    scroller.set_child(Some(&picture));
    viewer.set_child(Some(&scroller));

    let escape = gtk::EventControllerKey::new();
    let viewer_weak = viewer.downgrade();
    escape.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape {
            if let Some(viewer) = viewer_weak.upgrade() {
                viewer.close();
            }
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    viewer.add_controller(escape);
    viewer.present();
}

fn add_text(container: &gtk::Box, text: &str, class: &str) {
    add_markup(container, &glib::markup_escape_text(text), class);
}

fn add_markup(container: &gtk::Box, markup: &str, class: &str) -> gtk::Label {
    let label = gtk::Label::new(None);
    label.set_markup(markup);
    label.add_css_class(class);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_selectable(true);
    label.set_xalign(0.0);
    label.set_valign(gtk::Align::Start);
    container.append(&label);
    label
}

/// Normal external URLs still use GTK's default browser handler. Links to
/// the connected Jira instance are stopped here and dispatched inside the app.
fn add_rich_markup(
    container: &gtk::Box,
    markup: &str,
    class: &str,
    base_url: &str,
    on_issue_link: &OpenIssueLink,
) {
    let label = add_markup(container, markup, class);
    let base_url = base_url.to_owned();
    let on_issue_link = on_issue_link.clone();
    label.connect_activate_link(move |_, href| {
        if let Some(key) = issue_links::key_from_url(href, &base_url) {
            on_issue_link(&key);
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
}

fn render_block(
    parent: &gtk::Box,
    node: &Value,
    attachments: &[Attachment],
    used: &mut HashSet<String>,
    slots: &mut ImageSlots,
    base_url: &str,
    on_issue_link: &OpenIssueLink,
) {
    let kind = node.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "paragraph" => {
            let markup = inline_children(node, base_url);
            if !markup.is_empty() {
                add_rich_markup(parent, &markup, "issue-prose", base_url, on_issue_link);
            }
        }
        "heading" => {
            let level = node.get("attrs").and_then(|a| a.get("level"))
                .and_then(Value::as_u64).unwrap_or(2);
            let class = if level <= 2 { "issue-heading" } else { "issue-subheading" };
            add_rich_markup(parent, &inline_children(node, base_url), class, base_url, on_issue_link);
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
                    render_block(&content, child, attachments, used, slots, base_url, on_issue_link);
                }
                row.append(&content);
                parent.append(&row);
            }
        }
        "blockquote" | "panel" => {
            let box_ = gtk::Box::new(gtk::Orientation::Vertical, 8);
            box_.add_css_class(if kind == "panel" { "issue-panel" } else { "issue-quote" });
            for child in children(node) {
                render_block(&box_, child, attachments, used, slots, base_url, on_issue_link);
            }
            parent.append(&box_);
        }
        "codeBlock" => {
            let mut code = String::new();
            collect_text(node, &mut code);
            add_rich_markup(parent,
                &issue_links::markup_from_text(code.trim_end_matches('\n'), base_url),
                "issue-code", base_url, on_issue_link);
        }
        "rule" => parent.append(&gtk::Separator::new(gtk::Orientation::Horizontal)),
        "mediaSingle" | "mediaGroup" => {
            for child in children(node) {
                render_block(parent, child, attachments, used, slots, base_url, on_issue_link);
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
                        render_block(&cell_box, child, attachments, used, slots, base_url, on_issue_link);
                    }
                    row_box.append(&cell_box);
                }
                table.append(&row_box);
            }
            parent.append(&table);
        }
        _ => {
            for child in children(node) {
                render_block(parent, child, attachments, used, slots, base_url, on_issue_link);
            }
        }
    }
}

fn children(node: &Value) -> &[Value] {
    node.get("content").and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn inline_children(node: &Value, base_url: &str) -> String {
    children(node).iter().map(|child| inline_markup(child, base_url)).collect()
}

fn inline_markup(node: &Value, base_url: &str) -> String {
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
                let display = issue_links::key_from_url(url, base_url);
                let label = display.as_deref().unwrap_or(url);
                return format!("<a href=\"{}\">{}</a>",
                    glib::markup_escape_text(url),
                    if display.is_some() {
                        format!("<b>{}</b>", glib::markup_escape_text(label))
                    } else {
                        glib::markup_escape_text(label).to_string()
                    });
            }
            return glib::markup_escape_text(url).to_string();
        }
        _ => return inline_children(node, base_url),
    };
    let marks = node.get("marks").and_then(Value::as_array);
    let explicit_link = marks.into_iter().flatten().find_map(|mark| {
        (mark.get("type").and_then(Value::as_str) == Some("link"))
            .then(|| mark.get("attrs").and_then(|attrs| attrs.get("href"))
                .and_then(Value::as_str).filter(|url| safe_url(url)))
            .flatten()
    });
    // ADF links own their content: never nest another <a> inside them.
    // When Jira linked the whole URL, render the short issue key instead.
    let mut markup = if let Some(url) = explicit_link {
        let label = if text == url {
            issue_links::key_from_url(url, base_url).unwrap_or_else(|| text.to_owned())
        } else {
            text.to_owned()
        };
        glib::markup_escape_text(&label).to_string()
    } else {
        issue_links::markup_from_text(text, base_url)
    };
    if let Some(marks) = marks {
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
                        let content = if issue_links::key_from_url(url, base_url).is_some() {
                            format!("<b>{markup}</b>")
                        } else {
                            markup
                        };
                        markup = format!("<a href=\"{}\">{content}</a>",
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
    // Only use a stable REST attachment ID or an exact filename match.
    // Media UUIDs in ADF do not necessarily match REST attachment IDs. Guessing
    // from attachment order can put the wrong image into the issue description.
    if let Some(attachment) = attachments.iter().find(|a| a.is_image()
        && !used.contains(&a.id) && id == Some(a.id.as_str()))
    {
        return Some(attachment);
    }
    let filename = alt?;
    let mut matching = attachments.iter().filter(|attachment| {
        attachment.is_image() && attachment.filename.eq_ignore_ascii_case(filename)
    });
    let candidate = matching.next()?;
    // Duplicate filenames have no unique mapping to an ADF media UUID.
    if matching.next().is_some() || used.contains(&candidate.id) {
        None
    } else {
        Some(candidate)
    }
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
        slots.entry(attachment.id.clone()).or_default().push((slot.clone(), attachment.filename.clone()));
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
    base_url: &str,
    on_issue_link: &OpenIssueLink,
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
            add_rich_markup(parent, &issue_links::markup_from_text(heading, base_url),
                "issue-heading", base_url, on_issue_link);
        } else {
            add_rich_markup(parent, &issue_links::markup_from_text(paragraph, base_url),
                "issue-prose", base_url, on_issue_link);
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
    fn displays_jira_comment_time_without_milliseconds() {
        assert_eq!(format_comment_time("2026-11-08T13:20:01.000+0100"),
            "2026-11-08 · 13:20");
        assert_eq!(format_comment_time("unknown"), "unknown");
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
        let markup = inline_children(&node, "https://jira.example.com");
        assert!(markup.contains("<b>A &lt; B &amp; C</b>"));
        assert!(markup.contains("<a href=\"https://example.com/?a=1&amp;b=2\"> click</a>"));
        assert!(!markup.contains("javascript:"));
    }

    #[test]
    fn adf_issue_references_are_readable_and_clickable() {
        let site = "https://jira.example.com";
        let paragraph = json!({"type":"paragraph","content":[
            {"type":"text","text":"See ART-2272, "},
            {"type":"text","text":"https://jira.example.com/browse/AFB-42",
             "marks":[{"type":"link","attrs":{"href":"https://jira.example.com/browse/AFB-42"}}]},
            {"type":"text","text":" or this task",
             "marks":[{"type":"link","attrs":{"href":"https://jira.example.com/browse/ART-8"}}]},
            {"type":"inlineCard","attrs":{"url":"https://jira.example.com/browse/ART-9"}}
        ]});
        let markup = inline_children(&paragraph, site);
        assert!(markup.contains("<b>ART-2272</b>"));
        assert!(markup.contains("<b>AFB-42</b>"));
        assert!(!markup.contains(">https://jira.example.com/browse/AFB-42</a>"));
        assert!(markup.contains("<b> or this task</b>"));
        assert!(markup.contains("<b>ART-9</b>"));
    }

    #[test]
    fn adf_media_matches_exact_filename_not_attachment_order() {
        let attachments = vec![attachment("1", "first.png"), attachment("2", "target.png")];
        let node = json!({"type":"media","attrs":{"id":"not-an-attachment-id","alt":"target.png"}});
        let used = HashSet::new();
        assert_eq!(match_media(&node, &attachments, &used).unwrap().id, "2");
    }
    #[test]
    fn duplicate_attachment_filenames_are_not_guessed_for_inline_media() {
        let attachments = vec![attachment("1", "image.png"), attachment("2", "image.png")];
        let node = json!({"type":"media","attrs":{"id":"unmapped-uuid","alt":"image.png"}});
        assert!(match_media(&node, &attachments, &HashSet::new()).is_none());
        let exact = json!({"type":"media","attrs":{"id":"2","alt":"image.png"}});
        assert_eq!(match_media(&exact, &attachments, &HashSet::new()).unwrap().id, "2");
    }

    #[test]
    fn unresolved_adf_media_never_displays_an_unrelated_attachment() {
        let attachments = vec![attachment("1", "first.png"), attachment("2", "second.png")];
        let node = json!({"type":"media","attrs":{"id":"unrelated-media-uuid"}});
        assert!(match_media(&node, &attachments, &HashSet::new()).is_none());
        // Attachments are still displayed in their normal separate section.
    }

}
