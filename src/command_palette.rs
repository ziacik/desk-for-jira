use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    rc::Rc,
    sync::mpsc,
    thread,
    time::Duration,
};

use adw::prelude::*;
use gtk::glib;

use crate::jira::{Credentials, Issue, JiraClient};

#[derive(Clone)]
pub(crate) enum PaletteCommand {
    OpenIssue(Issue),
    FilterProject(Option<String>),
    ChooseProjects,
    EditColumns,
    Refresh,
    FocusBoardSearch,
    TogglePin(Issue),
    CopyBranch(Issue),
    OpenInJira(Issue),
}

#[derive(Clone)]
pub(crate) struct PaletteItem {
    pub title: String,
    pub subtitle: String,
    pub category: &'static str,
    pub icon: &'static str,
    pub featured: bool,
    pub command: PaletteCommand,
}

impl PaletteItem {
    pub fn new(
        title: impl Into<String>,
        subtitle: impl Into<String>,
        category: &'static str,
        icon: &'static str,
        featured: bool,
        command: PaletteCommand,
    ) -> Self {
        Self {
            title: title.into(),
            subtitle: subtitle.into(),
            category,
            icon,
            featured,
            command,
        }
    }

    fn matches(&self, needle: &str) -> bool {
        self.title.to_lowercase().contains(needle)
            || self.subtitle.to_lowercase().contains(needle)
            || self.category.to_lowercase().contains(needle)
    }
}

fn make_search_jql(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.chars().count() < 3 {
        return None;
    }

    if let Some((project, number)) = raw.rsplit_once('-')
        && !project.is_empty()
        && project.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && number.bytes().all(|b| b.is_ascii_digit())
        && !number.is_empty()
    {
        return Some(format!("key = {}-{} ORDER BY updated DESC", project.to_ascii_uppercase(), number));
    }

    // Only token characters may enter the JQL; neither quotes nor operators
    // from user input can turn this into an arbitrary Jira query.
    let terms: Vec<String> = raw
        .split_whitespace()
        .filter_map(|part| {
            let cleaned: String = part
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_')
                .take(40)
                .collect();
            (!cleaned.is_empty()).then_some(cleaned)
        })
        .take(4)
        .collect();
    if terms.is_empty() {
        return None;
    }
    let clauses: Vec<_> = terms.iter().map(|term| format!("summary ~ \"{term}\"")).collect();
    Some(format!("{} ORDER BY updated DESC", clauses.join(" AND ")))
}

fn render_results(
    list: &gtk::ListBox,
    hint: &gtk::Label,
    all_items: &[PaletteItem],
    remote_issues: &[Issue],
    query: &str,
    loading: bool,
    error: Option<&str>,
    shown_items: &RefCell<Vec<PaletteItem>>,
) {
    while let Some(row) = list.row_at_index(0) {
        list.remove(&row);
    }
    let needle = query.trim().to_lowercase();
    let mut matched: Vec<PaletteItem> = all_items
        .iter()
        .filter(|item| if needle.is_empty() { item.featured } else { item.matches(&needle) })
        .cloned()
        .collect();
    let mut keys: HashSet<String> = matched
        .iter()
        .filter_map(|item| match &item.command {
            PaletteCommand::OpenIssue(issue) => Some(issue.key.clone()),
            _ => None,
        })
        .collect();
    for issue in remote_issues {
        if keys.insert(issue.key.clone()) {
            matched.push(PaletteItem::new(
                &issue.key,
                &issue.summary,
                "Jira",
                "view-list-symbolic",
                false,
                PaletteCommand::OpenIssue(issue.clone()),
            ));
        }
    }
    matched.truncate(35);
    for item in &matched {
        let row = gtk::ListBoxRow::new();
        row.add_css_class("command-row");
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        content.set_margin_top(10);
        content.set_margin_bottom(10);
        content.set_margin_start(13);
        content.set_margin_end(13);

        let icon = gtk::Image::from_icon_name(item.icon);
        icon.add_css_class("command-row-icon");
        icon.set_valign(gtk::Align::Center);
        content.append(&icon);

        let labels = gtk::Box::new(gtk::Orientation::Vertical, 3);
        labels.set_hexpand(true);
        let title = gtk::Label::new(Some(&item.title));
        title.add_css_class("command-row-title");
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        labels.append(&title);
        let subtitle = gtk::Label::new(Some(&item.subtitle));
        subtitle.add_css_class("command-row-subtitle");
        subtitle.set_xalign(0.0);
        subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
        labels.append(&subtitle);
        content.append(&labels);

        let category = gtk::Label::new(Some(item.category));
        category.add_css_class("command-row-category");
        category.set_valign(gtk::Align::Center);
        content.append(&category);
        row.set_child(Some(&content));
        list.append(&row);
    }
    *shown_items.borrow_mut() = matched;
    if let Some(row) = list.row_at_index(0) {
        list.select_row(Some(&row));
    }
    let message = if let Some(error) = error {
        format!("Jira search failed: {error}")
    } else if loading {
        "Searching Jira…".to_owned()
    } else if shown_items.borrow().is_empty() {
        "No matching tickets or commands".to_owned()
    } else if needle.is_empty() {
        "Type to search Jira and run commands".to_owned()
    } else {
        format!("{} results", shown_items.borrow().len())
    };
    hint.set_text(&message);
}

pub(crate) fn open(
    parent: &adw::ApplicationWindow,
    all_items: Vec<PaletteItem>,
    account: Option<Credentials>,
    on_activate: Rc<dyn Fn(PaletteCommand)>,
) -> gtk::Window {
    let window = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .decorated(false)
        .default_width(610)
        .default_height(500)
        .build();
    window.add_css_class("command-palette");

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("command-palette-surface");
    let search_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    search_row.add_css_class("command-search-row");
    let symbol = gtk::Image::from_icon_name("system-search-symbolic");
    symbol.add_css_class("command-search-icon");
    search_row.append(&symbol);
    let entry = gtk::SearchEntry::new();
    entry.add_css_class("command-search");
    entry.set_placeholder_text(Some("Search tickets, projects and commands…"));
    entry.set_hexpand(true);
    search_row.append(&entry);
    let shortcut = gtk::Label::new(Some("ESC"));
    shortcut.add_css_class("command-shortcut");
    search_row.append(&shortcut);
    root.append(&search_row);

    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_vexpand(true);
    let list = gtk::ListBox::new();
    list.add_css_class("command-results");
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.set_activate_on_single_click(true);
    scroll.set_child(Some(&list));
    root.append(&scroll);

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    footer.add_css_class("command-footer");
    let hint = gtk::Label::new(None);
    hint.set_xalign(0.0);
    hint.set_ellipsize(gtk::pango::EllipsizeMode::End);
    hint.set_hexpand(true);
    footer.append(&hint);
    let navigation = gtk::Label::new(Some("↑↓ Navigate    ↵ Run"));
    navigation.add_css_class("command-navigation");
    footer.append(&navigation);
    root.append(&footer);
    window.set_child(Some(&root));

    let shown_items = Rc::new(RefCell::new(Vec::<PaletteItem>::new()));
    let remote_issues = Rc::new(RefCell::new(Vec::<Issue>::new()));
    let error = Rc::new(RefCell::new(None::<String>));
    let loading = Rc::new(Cell::new(false));
    let request_id = Rc::new(Cell::new(0_u64));
    let debounce = Rc::new(RefCell::new(None::<glib::SourceId>));
    let (sender, receiver) = mpsc::channel::<(u64, Result<Vec<Issue>, String>)>();
    let all_items = Rc::new(all_items);

    let update: Rc<dyn Fn()> = Rc::new({
        let list = list.clone();
        let hint = hint.clone();
        let entry = entry.clone();
        let shown_items = shown_items.clone();
        let remote_issues = remote_issues.clone();
        let error = error.clone();
        let loading = loading.clone();
        move || render_results(
            &list, &hint, &all_items, &remote_issues.borrow(),
            entry.text().as_str(), loading.get(), error.borrow().as_deref(),
            &shown_items,
        )
    });
    update();

    let choose: Rc<dyn Fn(i32)> = Rc::new({
        let shown_items = shown_items.clone();
        let window = window.downgrade();
        move |index| {
            let action = shown_items.borrow().get(index as usize).map(|item| item.command.clone());
            if let Some(action) = action {
                if let Some(window) = window.upgrade() {
                    window.close();
                }
                on_activate(action);
            }
        }
    });
    list.connect_row_activated({
        let choose = choose.clone();
        move |_, row| choose(row.index())
    });
    entry.connect_activate({
        let list = list.clone();
        let choose = choose.clone();
        move |_| {
            let index = list.selected_row().map(|row| row.index()).unwrap_or(0);
            choose(index);
        }
    });

    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed({
        let list = list.clone();
        let window = window.downgrade();
        move |_, key, _, _| match key {
            gtk::gdk::Key::Escape => {
                if let Some(window) = window.upgrade() {
                    window.close();
                }
                glib::Propagation::Stop
            }
            gtk::gdk::Key::Down | gtk::gdk::Key::Up => {
                let current = list.selected_row().map(|row| row.index()).unwrap_or(0);
                let next = if key == gtk::gdk::Key::Down { current + 1 } else { (current - 1).max(0) };
                if let Some(row) = list.row_at_index(next) {
                    list.select_row(Some(&row));
                    row.grab_focus();
                }
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    entry.add_controller(keys);

    entry.connect_search_changed({
        let remote_issues = remote_issues.clone();
        let error = error.clone();
        let loading = loading.clone();
        let request_id = request_id.clone();
        let debounce = debounce.clone();
        let update = update.clone();
        move |entry| {
            request_id.set(request_id.get().wrapping_add(1));
            let id = request_id.get();
            if let Some(source) = debounce.borrow_mut().take() {
                source.remove();
            }
            remote_issues.borrow_mut().clear();
            *error.borrow_mut() = None;
            let jql = make_search_jql(entry.text().as_str());
            loading.set(jql.is_some() && account.is_some());
            update();
            if let (Some(jql), Some(account)) = (jql, account.clone()) {
                let sender = sender.clone();
                let source = glib::timeout_add_local_once(Duration::from_millis(300), move || {
                    thread::spawn(move || {
                        let result = (|| -> anyhow::Result<Vec<Issue>> {
                            JiraClient::new(account)?.quick_search_issues(&jql)
                        })().map_err(|e| e.to_string());
                        let _ = sender.send((id, result));
                    });
                });
                *debounce.borrow_mut() = Some(source);
            }
        }
    });

    let closed = Rc::new(Cell::new(false));
    window.connect_close_request({
        let closed = closed.clone();
        let debounce = debounce.clone();
        move |_| {
            closed.set(true);
            if let Some(source) = debounce.borrow_mut().take() {
                source.remove();
            }
            glib::Propagation::Proceed
        }
    });
    glib::timeout_add_local(Duration::from_millis(90), {
        let update = update.clone();
        let remote_issues = remote_issues.clone();
        let error = error.clone();
        let loading = loading.clone();
        let request_id = request_id.clone();
        move || {
            if closed.get() {
                return glib::ControlFlow::Break;
            }
            while let Ok((id, result)) = receiver.try_recv() {
                if id != request_id.get() {
                    continue;
                }
                loading.set(false);
                match result {
                    Ok(issues) => *remote_issues.borrow_mut() = issues,
                    Err(message) => *error.borrow_mut() = Some(message),
                }
                update();
            }
            glib::ControlFlow::Continue
        }
    });
    window.present();
    entry.grab_focus();
    window
}

#[cfg(test)]
mod tests {
    use super::make_search_jql;

    #[test]
    fn searches_exact_jira_keys() {
        assert_eq!(make_search_jql("art-123"), Some("key = ART-123 ORDER BY updated DESC".to_owned()));
    }

    #[test]
    fn sanitizes_terms_before_building_jql() {
        assert_eq!(
            make_search_jql("foo\" OR project=SECRET"),
            Some("summary ~ \"foo\" AND summary ~ \"OR\" AND summary ~ \"projectSECRET\" ORDER BY updated DESC".to_owned()),
        );
        assert!(make_search_jql("ab").is_none());
        assert!(make_search_jql("\"''").is_none());
    }
}
