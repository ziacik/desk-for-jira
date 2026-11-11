//! Repository-wide worktree browser, independent of the selected Jira ticket.
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    path::Path,
    rc::Rc,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::Duration,
};

use adw::prelude::*;
use gtk::glib;

use crate::{
    git::{self, Worktree},
    jira::{Credentials, JiraClient},
    preferences::{self, GitRepository},
};

struct TreeSnapshot {
    tree: Worktree,
    primary: bool,
    dirty: Option<bool>,
}

struct RepositorySnapshot {
    repository: GitRepository,
    trees: Vec<TreeSnapshot>,
    error: Option<String>,
    available_bases: Vec<String>,
    recommended_base: String,
}

fn inspect_repositories() -> Result<(String, Vec<RepositorySnapshot>), String> {
    let settings = preferences::load_git().map_err(|error| error.to_string())?;
    let mut seen = HashSet::new();
    let mut snapshots = Vec::new();
    for repository in settings.repositories {
        let path = Path::new(&repository.path);
        match git::worktrees(path) {
            Ok(trees) => {
                let trees = trees.into_iter().enumerate()
                    .filter_map(|(index, tree)| {
                        // More than one registered repository can point at the
                        // same Git common directory. Show every path once.
                        if !seen.insert(tree.path.clone()) { return None; }
                        let dirty = git::status(&tree.path).ok().map(|s| !s.is_empty());
                        Some(TreeSnapshot { tree, primary: index == 0, dirty })
                    })
                    .collect();
                // Resolve merge targets off the GTK thread; never fetch or
                // modify the repository merely to display merge status.
                let available_bases = git::available_bases(path).unwrap_or_default()
                    .into_iter().filter(|base| base != "HEAD").collect();
                let recommended_base = git::recommended_base(path);
                snapshots.push(RepositorySnapshot {
                    repository, trees, error: None, available_bases, recommended_base,
                });
            }
            Err(error) => snapshots.push(RepositorySnapshot {
                repository, trees: Vec::new(), error: Some(error.to_string()),
                available_bases: Vec::new(), recommended_base: String::new(),
            }),
        }
    }
    Ok((settings.editor, snapshots))
}

fn label(value: &str, css: &str) -> gtk::Label {
    let item = gtk::Label::new(Some(value));
    item.set_xalign(0.0);
    item.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    item.add_css_class(css);
    item
}

fn clear(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn poll<T: 'static>(receiver: Receiver<T>, done: impl Fn(T) + 'static) {
    glib::timeout_add_local(Duration::from_millis(100), move || {
        match receiver.try_recv() {
            Ok(value) => { done(value); glib::ControlFlow::Break }
            Err(TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(TryRecvError::Disconnected) => glib::ControlFlow::Break,
        }
    });
}

/// Update merge badges without blocking GTK. Results from a stale
/// comparison selection must not replace a newer comparison.
fn update_merge_badge(
    indicator: &gtk::Label,
    result: Result<git::MergeStatus, String>,
    target: &str,
) {
    indicator.remove_css_class("worktree-global-merged");
    indicator.remove_css_class("worktree-global-unconfirmed");
    match result {
        Ok(git::MergeStatus::Merged) => {
            indicator.set_text("Merged");
            indicator.add_css_class("worktree-global-merged");
            indicator.set_tooltip_text(Some(&format!(
                "The branch tip is an ancestor of {target} in local Git history."
            )));
        }
        Ok(git::MergeStatus::NotConfirmed) => {
            indicator.set_text("Not confirmed");
            indicator.add_css_class("worktree-global-unconfirmed");
            indicator.set_tooltip_text(Some(&format!(
                "The branch tip is not an ancestor of {target}. A squash or rebase merge may still have incorporated its changes."
            )));
        }
        Err(error) => {
            indicator.set_text("Unavailable");
            indicator.set_tooltip_text(Some(&error));
        }
    }
}

fn check_merge_statuses(
    repository: &str,
    base: Option<&str>,
    badges: &Rc<Vec<(gtk::Label, String)>>,
    revision: &Rc<Cell<u64>>,
) {
    let current = revision.get().wrapping_add(1);
    revision.set(current);
    let Some(base) = base else {
        for (badge, _) in badges.iter() {
            badge.set_text("Choose base");
            badge.set_tooltip_text(Some("Choose a branch above to check which commits are merged"));
            badge.remove_css_class("worktree-global-merged");
            badge.remove_css_class("worktree-global-unconfirmed");
        }
        return;
    };
    for (badge, _) in badges.iter() {
        badge.set_text("Checking…");
        badge.set_tooltip_text(Some("Checking local commit ancestry"));
        badge.remove_css_class("worktree-global-merged");
        badge.remove_css_class("worktree-global-unconfirmed");
    }
    let path = repository.to_owned();
    let base_name = base.to_owned();
    let branches: Vec<String> = badges.iter().map(|(_, branch)| branch.clone()).collect();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let results = branches.iter().map(|branch| {
            git::merge_status(Path::new(&path), branch, &base_name)
                .map_err(|error| error.to_string())
        }).collect::<Vec<_>>();
        let _ = tx.send(results);
    });
    let badges = badges.clone();
    let revision = revision.clone();
    let base = base.to_owned();
    poll(rx, move |results| {
        if revision.get() != current { return; }
        for ((badge, _), result) in badges.iter().zip(results) {
            update_merge_badge(badge, result, &base);
        }
    });
}

struct Section {
    widget: gtk::Box,
    rows: Vec<(gtk::Box, String)>,
}

struct TicketButton {
    button: gtk::Button,
    status: gtk::Label,
}

/// Give distinct Jira workflow status names distinct, calm colors within the
/// current list. No guesses based on English status names or Jira project keys.
fn ticket_status_colors(statuses: &HashMap<String, String>) -> HashMap<String, usize> {
    let mut names: Vec<String> = statuses.values().cloned().collect();
    names.sort();
    names.dedup();
    names.into_iter().enumerate().map(|(index, name)| (name, index % 8)).collect()
}

fn render_ticket_status(
    button: &TicketButton, key: &str, state: &str, color: Option<usize>,
    tooltip: &str,
) {
    for index in 0..8 {
        button.button.remove_css_class(&format!("worktree-global-ticket-tone-{index}"));
    }
    if let Some(color) = color {
        button.button.add_css_class(&format!("worktree-global-ticket-tone-{color}"));
    }
    button.status.set_text(state);
    button.button.set_tooltip_text(Some(&format!("Open {key} in Desk for Jira — {tooltip}")));
}


struct Manager {
    window: gtk::Window,
    content: gtk::Box,
    message: gtk::Label,
    total: gtk::Label,
    spinner: gtk::Spinner,
    search: gtk::SearchEntry,
    sections: RefCell<Vec<Section>>,
    toast: adw::ToastOverlay,
    open_issue: Rc<dyn Fn(String)>,
    account: Option<Credentials>,
    ticket_badges: RefCell<HashMap<String, Vec<TicketButton>>>,
    ticket_request: Cell<u64>,
}

impl Manager {
    fn refresh(self: &Rc<Self>) {
        // A new refresh invalidates pending Jira results from the previous view.
        self.ticket_request.set(self.ticket_request.get().wrapping_add(1));
        self.ticket_badges.borrow_mut().clear();
        clear(&self.content);
        self.sections.borrow_mut().clear();
        self.message.set_text("Checking local Git worktrees…");
        self.total.set_text("");
        self.spinner.start();
        let generation = self.ticket_request.get();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || { let _ = tx.send(inspect_repositories()); });
        let manager = self.clone();
        poll(rx, move |result| {
            if manager.ticket_request.get() != generation { return; }
            manager.spinner.stop();
            match result {
                Ok((editor, snapshots)) => {
                    manager.show_snapshots(&editor, snapshots);
                    manager.load_ticket_statuses();
                }
                Err(error) => manager.message.set_text(&error),
            }
        });
    }

    /// Load current Jira workflow statuses once per window refresh. The
    /// registered worktrees can reference tickets not present on any board.
    /// Never block GTK or infer a status from a Git branch.
    fn load_ticket_statuses(self: &Rc<Self>) {
        let keys: Vec<String> = self.ticket_badges.borrow().keys().cloned().collect();
        if keys.is_empty() { return; }
        let Some(account) = self.account.clone() else {
            for (key, badges) in self.ticket_badges.borrow().iter() {
                for badge in badges {
                    render_ticket_status(badge, key, "Disconnected", None,
                        "Connect to Jira to see the current ticket status");
                }
            }
            return;
        };
        let generation = self.ticket_request.get();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = JiraClient::new(account)
                .and_then(|client| client.issue_statuses_by_keys(&keys))
                .map_err(|error| error.to_string());
            let _ = tx.send(result);
        });
        let manager = self.clone();
        poll(rx, move |result| {
            if manager.ticket_request.get() != generation { return; }
            let colors = result.as_ref().map(ticket_status_colors).unwrap_or_default();
            let badges = manager.ticket_badges.borrow();
            for (key, widgets) in badges.iter() {
                let (state, color, tooltip) = match &result {
                    Ok(statuses) => match statuses.get(key) {
                        Some(status) => (status.as_str(), colors.get(status).copied(),
                            format!("Jira status: {status}")),
                        None => ("Unavailable", None,
                            format!("{key} not found or not accessible in Jira")),
                    },
                    Err(error) => ("Unavailable", None,
                        format!("Unable to read {key}: {error}")),
                };
                for widget in widgets {
                    render_ticket_status(widget, key, state, color, &tooltip);
                }
            }
        });
    }

    fn show_snapshots(self: &Rc<Self>, editor: &str, snapshots: Vec<RepositorySnapshot>) {
        clear(&self.content);
        let count = snapshots.iter().flat_map(|r| &r.trees).filter(|t| !t.primary).count();
        self.total.set_text(&format!("{count} linked worktrees"));
        self.message.set_text(if snapshots.is_empty() {
            "No repositories registered. Register a repository from a ticket's Git worktree dialog."
        } else if count == 0 {
            "No linked worktrees found. Primary checkouts are shown for reference."
        } else {
            ""
        });
        let mut sections = Vec::new();
        for snapshot in snapshots {
            let section = gtk::Box::new(gtk::Orientation::Vertical, 9);
            section.add_css_class("worktree-global-section");

            let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let icon = gtk::Image::from_icon_name("folder-symbolic");
            header.append(&icon);
            let title = label(&snapshot.repository.name, "worktree-global-repository");
            title.set_hexpand(true);
            title.set_tooltip_text(Some(&snapshot.repository.path));
            header.append(&title);
            let number = label(
                &format!("{} linked", snapshot.trees.iter().filter(|tree| !tree.primary).count()),
                "worktree-global-count",
            );
            header.append(&number);
            section.append(&header);

            let compare_row = gtk::Box::new(gtk::Orientation::Horizontal, 9);
            compare_row.add_css_class("worktree-global-merge-control");
            let compare_caption = label("Merge status vs", "dim-label");
            compare_caption.set_tooltip_text(Some(
                "Checks local Git commit ancestry. Fetch the repository to update remote-tracking branches."
            ));
            compare_row.append(&compare_caption);
            let mut choices = vec!["Choose branch…".to_owned()];
            choices.extend(snapshot.available_bases.iter().cloned());
            let select_base = gtk::DropDown::from_strings(
                &choices.iter().map(String::as_str).collect::<Vec<_>>());
            let selected = snapshot.available_bases.iter()
                .position(|name| name == &snapshot.recommended_base)
                .map(|index| index as u32 + 1).unwrap_or(0);
            select_base.set_selected(selected);
            select_base.set_sensitive(!snapshot.available_bases.is_empty());
            select_base.add_css_class("worktree-global-base-selector");
            compare_row.append(&select_base);
            section.append(&compare_row);

            if let Some(error) = snapshot.error {
                let note = label(&error, "worktree-global-error");
                note.set_wrap(true);
                section.append(&note);
            }
            let mut rows = Vec::new();
            let mut badges = Vec::new();
            for tree in snapshot.trees {
                let search_value = format!("{} {} {}",
                    snapshot.repository.name, tree.tree.branch.as_deref().unwrap_or(""),
                    tree.tree.path.display()).to_lowercase();
                let (row, merge_badge) = self.tree_row(&snapshot.repository, editor, tree);
                if let Some(badge) = merge_badge {
                    badges.push(badge);
                }
                section.append(&row);
                rows.push((row, search_value));
            }
            let badges = Rc::new(badges);
            let revision = Rc::new(Cell::new(0_u64));
            let repository_path = snapshot.repository.path.clone();
            let bases = snapshot.available_bases;
            {
                let badges = badges.clone();
                let revision = revision.clone();
                select_base.connect_selected_notify(move |dropdown| {
                    let base = (dropdown.selected() as usize).checked_sub(1)
                        .and_then(|index| bases.get(index));
                    check_merge_statuses(&repository_path, base.map(String::as_str),
                        &badges, &revision);
                });
            }
            let base = (select_base.selected() as usize).checked_sub(1)
                .and_then(|index| choices.get(index + 1));
            check_merge_statuses(&snapshot.repository.path, base.map(String::as_str),
                &badges, &revision);
            self.content.append(&section);
            sections.push(Section { widget: section, rows });
        }
        *self.sections.borrow_mut() = sections;
        self.filter();
    }

    fn filter(&self) {
        let needle = self.search.text().to_lowercase();
        for section in self.sections.borrow().iter() {
            let mut matching = 0;
            for (row, searchable) in &section.rows {
                let visible = needle.is_empty() || searchable.contains(needle.as_str());
                row.set_visible(visible);
                if visible { matching += 1; }
            }
            section.widget.set_visible(matching > 0 || (needle.is_empty() && section.rows.is_empty()));
        }
    }

    fn tree_row(
        self: &Rc<Self>, repo: &GitRepository, editor: &str, snapshot: TreeSnapshot,
    ) -> (gtk::Box, Option<(gtk::Label, String)>) {
        let row = gtk::Box::new(gtk::Orientation::Vertical, 7);
        row.add_css_class("worktree-global-entry");

        let top = gtk::Box::new(gtk::Orientation::Horizontal, 9);
        let name = if snapshot.primary {
            "Main checkout".to_owned()
        } else {
            snapshot.tree.branch.clone().unwrap_or_else(|| "Detached HEAD".to_owned())
        };
        let branch = label(&name, "worktree-global-branch");
        branch.set_hexpand(true);
        branch.set_tooltip_text(Some(&name));
        top.append(&branch);
        // The link is inferred from the same Jira-key prefix convention as
        // the issue detail. There is no extra persisted mapping.
        if let Some(key) = snapshot.tree.branch.as_deref().and_then(git::ticket_key_from_branch) {
            let ticket = gtk::Button::new();
            ticket.add_css_class("worktree-global-ticket-link");
            ticket.set_tooltip_text(Some(&format!("Open {key} in Desk for Jira")));
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let key_label = label(&key, "worktree-global-ticket-key");
            content.append(&key_label);
            content.append(&label("·", "worktree-global-ticket-divider"));
            let jira_status = label("Loading…", "worktree-global-ticket-state");
            jira_status.set_max_width_chars(25);
            jira_status.set_ellipsize(gtk::pango::EllipsizeMode::End);
            content.append(&jira_status);
            ticket.set_child(Some(&content));
            let open_issue = self.open_issue.clone();
            let window = self.window.downgrade();
            let ticket_key = key.clone();
            ticket.connect_clicked(move |_| {
                if let Some(window) = window.upgrade() {
                    window.close();
                }
                open_issue(ticket_key.clone());
            });
            top.append(&ticket);
            self.ticket_badges.borrow_mut().entry(key)
                .or_default().push(TicketButton { button: ticket, status: jira_status });
        }
        let status = if !snapshot.tree.path.exists() {
            "Missing"
        } else {
            match snapshot.dirty {
                Some(false) => "Clean",
                Some(true) => "Uncommitted changes",
                None => "Status unavailable",
            }
        };
        let indicator = label(status, "worktree-global-status");
        if snapshot.dirty == Some(true) { indicator.add_css_class("worktree-global-dirty"); }
        top.append(&indicator);
        let merge_badge = if !snapshot.primary {
            snapshot.tree.branch.as_ref().map(|branch| {
                let badge = label("Checking…", "worktree-global-merge-status");
                badge.set_tooltip_text(Some("Checking local Git commit ancestry"));
                top.append(&badge);
                (badge, branch.clone())
            })
        } else {
            None
        };
        row.append(&top);

        let path_row = gtk::Box::new(gtk::Orientation::Horizontal, 7);
        let path_text = snapshot.tree.path.to_string_lossy().to_string();
        let path = label(&path_text, "worktree-global-path");
        path.set_hexpand(true);
        path.set_selectable(true);
        path.set_tooltip_text(Some(&path_text));
        path_row.append(&path);
        let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
        copy.add_css_class("flat");
        copy.set_tooltip_text(Some("Copy full path"));
        let toast = self.toast.clone();
        copy.connect_clicked(move |_| {
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&path_text);
                toast.add_toast(adw::Toast::new("Worktree path copied"));
            }
        });
        path_row.append(&copy);
        row.append(&path_row);

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.add_css_class("worktree-global-actions");
        let editor_cmd = if editor == "codium" { "codium" } else { "code" };
        let open = gtk::Button::with_label(if editor_cmd == "codium" {
            "Open in VSCodium"
        } else {
            "Open in VS Code"
        });
        open.add_css_class("transition-action");
        open.set_sensitive(snapshot.tree.path.exists());
        let open_path = snapshot.tree.path.clone();
        let toast = self.toast.clone();
        open.connect_clicked(move |_| {
            if let Err(error) = git::open_editor(&open_path, editor_cmd) {
                toast.add_toast(adw::Toast::new(&error.to_string()));
            }
        });
        buttons.append(&open);

        let terminal = gtk::Button::with_label("Terminal");
        terminal.add_css_class("transition-action");
        terminal.set_sensitive(snapshot.tree.path.exists());
        let terminal_path = snapshot.tree.path.clone();
        let toast = self.toast.clone();
        terminal.connect_clicked(move |_| {
            if let Err(error) = git::open_terminal(&terminal_path) {
                toast.add_toast(adw::Toast::new(&error.to_string()));
            }
        });
        buttons.append(&terminal);

        if !snapshot.primary && snapshot.tree.branch.is_some() {
            let remove = gtk::Button::with_label("Remove…");
            remove.add_css_class("transition-action");
            remove.set_sensitive(snapshot.tree.path.exists());
            let repo = repo.clone();
            let tree = snapshot.tree;
            let manager = self.clone();
            remove.connect_clicked(move |_| {
                manager.confirm_removal(repo.clone(), tree.clone());
            });
            buttons.append(&remove);
        }
        row.append(&buttons);
        (row, merge_badge)
    }

    fn confirm_removal(self: &Rc<Self>, repo: GitRepository, tree: Worktree) {
        let confirm = gtk::Window::builder()
            .title("Remove worktree and local branch")
            .transient_for(&self.window).modal(true)
            .default_width(480).default_height(340).build();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_start(18);
        content.set_margin_end(18);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        let branch = tree.branch.as_deref().unwrap_or("Detached HEAD");
        let heading = label(&format!("Remove {branch}?"), "title-3");
        content.append(&heading);
        let explanation = label(
            "This removes the linked worktree and its local branch, but only if the worktree is clean and the branch is provably merged. Ignored files (such as node_modules) may be deleted. Remote branches are never deleted.",
            "dim-label",
        );
        explanation.set_wrap(true);
        explanation.set_ellipsize(gtk::pango::EllipsizeMode::None);
        content.append(&explanation);
        let target_label = label("Merged into", "dim-label");
        content.append(&target_label);
        let bases = git::available_bases(Path::new(&repo.path)).unwrap_or_default();
        let target = gtk::DropDown::from_strings(
            &bases.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        let recommended = git::recommended_base(Path::new(&repo.path));
        if let Some(index) = bases.iter().position(|r| r == &recommended) {
            target.set_selected(index as u32);
        }
        target.set_sensitive(!bases.is_empty());
        content.append(&target);
        let progress = label("", "worktree-global-error");
        progress.set_wrap(true);
        progress.set_visible(false);
        content.append(&progress);

        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 9);
        footer.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        let remove = gtk::Button::with_label("Remove worktree and branch");
        remove.add_css_class("destructive-action");
        remove.set_sensitive(!bases.is_empty());
        footer.append(&cancel);
        footer.append(&remove);
        content.append(&footer);
        confirm.set_child(Some(&content));
        {
            let confirm = confirm.clone();
            cancel.connect_clicked(move |_| confirm.close());
        }
        {
            let confirm = confirm.clone();
            let manager = self.clone();
            remove.clone().connect_clicked(move |_| {
                let selected = target.selected() as usize;
                let Some(target_branch) = bases.get(selected).cloned() else {
                    progress.set_text("Choose a merge target.");
                    progress.set_visible(true);
                    return;
                };
                remove.set_sensitive(false);
                target.set_sensitive(false);
                progress.set_text("Verifying Git history and worktree status…");
                progress.set_visible(true);
                let repo_path = repo.path.clone();
                let tree = tree.clone();
                let (tx, rx) = mpsc::channel();
                thread::spawn(move || {
                    let result = git::remove_merged_worktree(Path::new(&repo_path),
                        &tree, &target_branch).map_err(|e| e.to_string());
                    let _ = tx.send(result);
                });
                let manager = manager.clone();
                let confirm = confirm.clone();
                let remove = remove.clone();
                let target = target.clone();
                let progress = progress.clone();
                poll(rx, move |result| {
                    match result {
                        Ok(()) => {
                            confirm.close();
                            manager.toast.add_toast(adw::Toast::new("Worktree and local branch removed"));
                            manager.refresh();
                        }
                        Err(error) => {
                            progress.set_text(&error);
                            target.set_sensitive(true);
                            remove.set_sensitive(true);
                        }
                    }
                });
            });
        }
        confirm.present();
    }
}

/// Open a repository-wide overview from the main board header, regardless of
/// the selected Jira ticket. This enumerates the configured repositories,
/// not the currently loaded Jira issues.
pub fn show(
    parent: &adw::ApplicationWindow,
    toast: &adw::ToastOverlay,
    open_issue: Rc<dyn Fn(String)>,
    layout_settings: Rc<RefCell<preferences::UiLayout>>,
    account: Option<Credentials>,
) {
    let (width, height) = {
        let settings = layout_settings.borrow();
        (settings.worktrees_window_width, settings.worktrees_window_height)
    };
    let window = gtk::Window::builder().title("Worktrees")
        .transient_for(parent).default_width(width).default_height(height).build();
    window.add_css_class("worktree-global-window");
    window.connect_close_request(move |window| {
        // Save the actual allocated size (not the requested default size).
        // Using the main UI layout state avoids later layout saves reverting it.
        if !window.is_maximized() && window.width() >= 640 && window.height() >= 480 {
            let mut settings = layout_settings.borrow_mut();
            settings.worktrees_window_width = window.width();
            settings.worktrees_window_height = window.height();
            if let Err(error) = preferences::save_layout(&settings) {
                eprintln!("Could not save Worktrees window size: {error}");
            }
        }
        glib::Propagation::Proceed
    });

    let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    toolbar.add_css_class("worktree-global-toolbar");
    let title = label("All worktrees", "title-2");
    title.set_hexpand(true);
    toolbar.append(&title);
    let count = label("", "dim-label");
    toolbar.append(&count);
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.add_css_class("flat");
    refresh.set_tooltip_text(Some("Refresh worktree list"));
    toolbar.append(&refresh);
    layout.append(&toolbar);

    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Filter by repository, branch or path…"));
    search.add_css_class("worktree-global-search");
    layout.append(&search);
    let message = label("", "dim-label");
    message.add_css_class("worktree-global-message");
    message.set_wrap(true);
    layout.append(&message);
    let spinner = gtk::Spinner::new();
    spinner.set_halign(gtk::Align::Center);
    layout.append(&spinner);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_vexpand(true);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 13);
    content.add_css_class("worktree-global-content");
    scroll.set_child(Some(&content));
    layout.append(&scroll);
    window.set_child(Some(&layout));

    let manager = Rc::new(Manager {
        window: window.clone(), content, message, total: count,
        spinner, search: search.clone(), sections: RefCell::new(Vec::new()),
        toast: toast.clone(), open_issue, account,
        ticket_badges: RefCell::new(HashMap::new()),
        ticket_request: Cell::new(0),
    });
    {
        let manager = manager.clone();
        refresh.connect_clicked(move |_| manager.refresh());
    }
    {
        let manager = manager.clone();
        search.connect_search_changed(move |_| manager.filter());
    }
    manager.refresh();
    window.present();
}
