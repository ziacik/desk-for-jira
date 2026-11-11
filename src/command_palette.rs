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

use crate::jira::{Credentials, Issue, IssueTransition, JiraClient};

#[derive(Clone)]
pub(crate) enum PaletteCommand {
    OpenIssue(Issue),
    OpenIssueByKey(String),
    Transition { issue_key: String, transition: IssueTransition },
    FilterProject(Option<String>),
    ChooseProjects,
    EditColumns,
    Refresh,
    FocusBoardSearch,
    TogglePin(Issue),
    ManageWorktree(Issue),
    AllWorktrees,
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

#[derive(Default)]
struct PaletteStatus {
    search_loading: bool,
    search_error: Option<String>,
    transitions_loading: bool,
    transitions_error: Option<String>,
}

fn transition_items(key: &str, transitions: Vec<IssueTransition>) -> Vec<PaletteItem> {
    transitions.into_iter().map(|transition| {
        let title = if transition.fields.iter().any(|field| field.required) {
            format!("{}…", transition.name)
        } else {
            transition.name.clone()
        };
        let subtitle = format!("{key} · Move to {}", transition.to_name);
        PaletteItem::new(
            title, subtitle, "Transition", "go-next-symbolic", true,
            PaletteCommand::Transition {
                issue_key: key.to_owned(),
                transition,
            },
        )
    }).collect()
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

/// Offer a direct lookup when a number did not match a loaded or searched issue.
/// These are candidate keys, not Jira results: existence is checked on activation.
fn issue_number_candidates(query: &str, projects: &[String]) -> Vec<PaletteItem> {
    let number = query.trim();
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Vec::new();
    }
    let mut seen = HashSet::new();
    projects.iter().filter_map(|project| {
        let project = project.trim().to_ascii_uppercase();
        if project.is_empty()
            || !project.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            || !seen.insert(project.clone())
        {
            return None;
        }
        let key = format!("{project}-{number}");
        Some(PaletteItem::new(
            &key, "Open directly from Jira · not yet verified",
            "Open ticket", "document-open-symbolic", false,
            PaletteCommand::OpenIssueByKey(key.clone()),
        ))
    }).collect()
}

struct PaletteQuery<'a> {
    text: &'a str,
    project_keys: &'a [String],
}

fn render_results(
    list: &gtk::ListBox,
    hint: &gtk::Label,
    all_items: &[PaletteItem],
    remote_issues: &[Issue],
    query: PaletteQuery<'_>,
    status: &PaletteStatus,
    shown_items: &RefCell<Vec<PaletteItem>>,
) {
    let previous_selection = list.selected_row().and_then(|row| {
        shown_items.borrow().get(row.index() as usize)
            .map(|item| (item.title.clone(), item.subtitle.clone(), item.category))
    });
    while let Some(row) = list.row_at_index(0) {
        list.remove(&row);
    }
    let needle = query.text.trim().to_lowercase();
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
    let mut suggested_direct_open = false;
    if matched.is_empty() && !status.search_loading {
        let candidates = issue_number_candidates(query.text, query.project_keys);
        suggested_direct_open = !candidates.is_empty();
        matched.extend(candidates);
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
    let selected_index = previous_selection.and_then(|(title, subtitle, category)| {
        matched.iter().position(|item| {
            item.title == title && item.subtitle == subtitle && item.category == category
        })
    }).unwrap_or(0);
    *shown_items.borrow_mut() = matched;
    if let Some(row) = list.row_at_index(selected_index as i32) {
        list.select_row(Some(&row));
    }
    let message = if suggested_direct_open {
        "Possible ticket keys · verified in Jira when opened".to_owned()
    } else if let Some(error) = status.search_error.as_deref() {
        format!("Jira search failed: {error}")
    } else if status.search_loading {
        "Searching Jira…".to_owned()
    } else if needle.is_empty() && status.transitions_loading {
        "Loading available Jira transitions…".to_owned()
    } else if needle.is_empty() && let Some(error) = status.transitions_error.as_deref() {
        format!("Could not load transitions: {error}")
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
    project_keys: Vec<String>,
    selected_key: Option<String>,
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
    let status = Rc::new(RefCell::new(PaletteStatus {
        transitions_loading: selected_key.is_some() && account.is_some(),
        ..PaletteStatus::default()
    }));
    let request_id = Rc::new(Cell::new(0_u64));
    let debounce = Rc::new(RefCell::new(None::<glib::SourceId>));
    let (sender, receiver) = mpsc::channel::<(u64, Result<Vec<Issue>, String>)>();
    let (transition_sender, transition_receiver) = mpsc::channel::<(String, Result<Vec<IssueTransition>, String>)>();
    if let (Some(key), Some(account)) = (selected_key, account.clone()) {
        thread::spawn(move || {
            let result = (|| -> anyhow::Result<Vec<IssueTransition>> {
                JiraClient::new(account)?.issue_transitions(&key)
            })().map_err(|error| error.to_string());
            let _ = transition_sender.send((key, result));
        });
    }
    let all_items = Rc::new(RefCell::new(all_items));
    let project_keys = Rc::new(project_keys);

    let update: Rc<dyn Fn()> = Rc::new({
        let all_items = all_items.clone();
        let list = list.clone();
        let hint = hint.clone();
        let entry = entry.downgrade();
        let shown_items = shown_items.clone();
        let remote_issues = remote_issues.clone();
        let project_keys = project_keys.clone();
        let status = status.clone();
        move || {
            let Some(entry) = entry.upgrade() else { return; };
            render_results(
                &list, &hint, &all_items.borrow(), &remote_issues.borrow(),
                PaletteQuery { text: entry.text().as_str(), project_keys: &project_keys },
                &status.borrow(), &shown_items,
            );
        }
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
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
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
                    // Keep focus in SearchEntry so subsequent typing edits the query.
                }
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    entry.add_controller(keys);

    entry.connect_search_changed({
        let remote_issues = remote_issues.clone();
        let status = status.clone();
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
            let jql = make_search_jql(entry.text().as_str());
            {
                let mut status = status.borrow_mut();
                status.search_error = None;
                status.search_loading = jql.is_some() && account.is_some();
            }
            update();
            if let (Some(jql), Some(account)) = (jql, account.clone()) {
                let sender = sender.clone();
                let finished_timer = debounce.clone();
                let source = glib::timeout_add_local_once(Duration::from_millis(300), move || {
                    finished_timer.borrow_mut().take();
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
        let status = status.clone();
        let all_items = all_items.clone();
        let request_id = request_id.clone();
        move || {
            if closed.get() {
                return glib::ControlFlow::Break;
            }
            while let Ok((id, result)) = receiver.try_recv() {
                if id != request_id.get() {
                    continue;
                }
                status.borrow_mut().search_loading = false;
                match result {
                    Ok(issues) => *remote_issues.borrow_mut() = issues,
                    Err(message) => status.borrow_mut().search_error = Some(message),
                }
                update();
            }
            while let Ok((key, result)) = transition_receiver.try_recv() {
                let mut status = status.borrow_mut();
                status.transitions_loading = false;
                match result {
                    Ok(transitions) => {
                        let actions = transition_items(&key, transitions);
                        let mut items = all_items.borrow_mut();
                        let index = items.iter().position(|item| {
                            matches!(item.command, PaletteCommand::OpenIssue(_))
                        }).unwrap_or(items.len());
                        items.splice(index..index, actions);
                    }
                    Err(message) => status.transitions_error = Some(message),
                }
                drop(status);
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
    use super::{PaletteCommand, issue_number_candidates, make_search_jql, transition_items};
    use crate::jira::{IssueTransition, TransitionField};

    #[test]
    fn searches_exact_jira_keys() {
        assert_eq!(make_search_jql("art-123"), Some("key = ART-123 ORDER BY updated DESC".to_owned()));
    }

    #[test]
    fn numeric_queries_offer_direct_lookup_in_selected_projects() {
        let projects = vec!["art".to_owned(), "AFB".to_owned(), "ART".to_owned()];
        let items = issue_number_candidates(" 2272 ", &projects);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "ART-2272");
        assert_eq!(items[1].title, "AFB-2272");
        assert!(matches!(
            &items[0].command,
            PaletteCommand::OpenIssueByKey(key) if key == "ART-2272"
        ));
        assert!(issue_number_candidates("ART-2272", &projects).is_empty());
        assert!(issue_number_candidates("22 72", &projects).is_empty());
        assert!(issue_number_candidates("", &projects).is_empty());
    }

    #[test]
    fn transition_palette_keeps_real_workflow_ids_and_required_fields() {
        let first = IssueTransition {
            id: "21".to_owned(),
            name: "Start progress".to_owned(),
            to_name: "In Progress".to_owned(),
            to_id: "3".to_owned(),
            fields: vec![],
            edit_fields: vec![],
        };
        let second = IssueTransition {
            id: "42".to_owned(),
            name: "Resolve".to_owned(),
            to_name: "Done".to_owned(),
            to_id: "10001".to_owned(),
            fields: vec![TransitionField {
                id: "resolution".to_owned(),
                name: "Resolution".to_owned(),
                schema_type: "resolution".to_owned(),
                item_type: String::new(),
                custom_type: String::new(),
                required: true,
                choices: vec![],
            }],
            edit_fields: vec![],
        };
        let items = transition_items("ART-123", vec![first, second]);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Start progress");
        assert_eq!(items[1].title, "Resolve…");
        assert_eq!(items[1].subtitle, "ART-123 · Move to Done");
        assert!(items[1].matches("resolve"));
        assert!(items[1].matches("transition"));
        match &items[1].command {
            PaletteCommand::Transition { issue_key, transition } => {
                assert_eq!(issue_key, "ART-123");
                assert_eq!(transition.id, "42");
                assert_eq!(transition.fields.len(), 1);
            }
            _ => panic!("Transition mapped to an unrelated palette command"),
        }
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
