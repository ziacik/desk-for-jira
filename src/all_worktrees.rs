//! Repository-wide worktree browser, independent of the selected Jira ticket.
use std::{
    cell::RefCell,
    collections::HashSet,
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
                snapshots.push(RepositorySnapshot {
                    repository, trees, error: None,
                });
            }
            Err(error) => snapshots.push(RepositorySnapshot {
                repository, trees: Vec::new(), error: Some(error.to_string()),
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

struct Section {
    widget: gtk::Box,
    rows: Vec<(gtk::Box, String)>,
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
}

impl Manager {
    fn refresh(self: &Rc<Self>) {
        clear(&self.content);
        self.sections.borrow_mut().clear();
        self.message.set_text("Checking local Git worktrees…");
        self.total.set_text("");
        self.spinner.start();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || { let _ = tx.send(inspect_repositories()); });
        let manager = self.clone();
        poll(rx, move |result| {
            manager.spinner.stop();
            match result {
                Ok((editor, snapshots)) => manager.show_snapshots(&editor, snapshots),
                Err(error) => manager.message.set_text(&error),
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

            if let Some(error) = snapshot.error {
                let note = label(&error, "worktree-global-error");
                note.set_wrap(true);
                section.append(&note);
            }
            let mut rows = Vec::new();
            for tree in snapshot.trees {
                let search_value = format!("{} {} {}",
                    snapshot.repository.name, tree.tree.branch.as_deref().unwrap_or(""),
                    tree.tree.path.display()).to_lowercase();
                let row = self.tree_row(&snapshot.repository, editor, tree);
                section.append(&row);
                rows.push((row, search_value));
            }
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

    fn tree_row(self: &Rc<Self>, repo: &GitRepository, editor: &str, snapshot: TreeSnapshot) -> gtk::Box {
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
            let ticket = gtk::Button::with_label(&key);
            ticket.add_css_class("flat");
            ticket.add_css_class("worktree-global-ticket-link");
            ticket.set_tooltip_text(Some(&format!("Open {key} in Desk for Jira")));
            let open_issue = self.open_issue.clone();
            let window = self.window.downgrade();
            ticket.connect_clicked(move |_| {
                if let Some(window) = window.upgrade() {
                    window.close();
                }
                open_issue(key.clone());
            });
            top.append(&ticket);
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
        row
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
) {
    let window = gtk::Window::builder().title("Worktrees")
        .transient_for(parent).default_width(760).default_height(620).build();
    window.add_css_class("worktree-global-window");

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
        toast: toast.clone(), open_issue,
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
