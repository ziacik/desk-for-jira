//! Visible, read-only Git state for the currently selected Jira ticket.
//! The action text is determined from Git state *before* the user clicks.
use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc::{self, TryRecvError},
    thread,
    time::Duration,
};

use adw::prelude::*;
use gtk::glib;

use crate::{git, git_ui, issue_detail, preferences};

#[derive(Clone, Debug)]
struct ExistingWorktree {
    repository: String,
    branch: String,
    path: PathBuf,
    dirty: Option<bool>,
}

#[derive(Clone, Debug)]
enum Snapshot {
    Unconfigured,
    Missing {
        repository: String,
        branch: String,
        destination: String,
        base: String,
        matching_branches: usize,
    },
    Existing {
        editor: String,
        worktrees: Vec<ExistingWorktree>,
    },
}

fn inspect(site: &str, key: &str, summary: &str) -> Result<Snapshot, String> {
    let settings = preferences::load_git().map_err(|error| error.to_string())?;
    if settings.repositories.is_empty() {
        return Ok(Snapshot::Unconfigured);
    }

    let mut existing = Vec::new();
    let mut available = Vec::new();
    let mut errors = Vec::new();
    for (index, repo) in settings.repositories.iter().enumerate() {
        let path = Path::new(&repo.path);
        if !git::is_repository(path) {
            errors.push(format!("{} is not an accessible Git repository", repo.name));
            continue;
        }
        match git::matching_worktrees(path, key) {
            Ok(trees) => {
                for tree in trees {
                    existing.push(ExistingWorktree {
                        repository: repo.name.clone(),
                        branch: tree.branch.unwrap_or_else(|| "Detached HEAD".to_owned()),
                        dirty: git::status(&tree.path).ok().map(|s| !s.is_empty()),
                        path: tree.path,
                    });
                }
                available.push(index);
            }
            Err(error) => errors.push(format!("{}: {error}", repo.name)),
        }
    }
    if !existing.is_empty() {
        return Ok(Snapshot::Existing {
            editor: settings.editor,
            worktrees: existing,
        });
    }

    let preferred = settings.preferred_index(site, key);
    let index = preferred
        .filter(|index| available.contains(index))
        .or_else(|| available.first().copied());
    let Some(index) = index else {
        return Err(if errors.is_empty() {
            "No accessible Git repositories".to_owned()
        } else {
            errors.join("\n")
        });
    };
    let repo = &settings.repositories[index];
    let branches = git::matching_branches(Path::new(&repo.path), key)
        .map_err(|error| error.to_string())?;
    Ok(Snapshot::Missing {
        repository: repo.name.clone(),
        branch: branches.first().cloned()
            .unwrap_or_else(|| issue_detail::suggested_branch_name(key, summary)),
        destination: Path::new(&settings.worktree_root).join(&repo.name).join(key)
            .to_string_lossy().into_owned(),
        base: git::recommended_base(Path::new(&repo.path)),
        matching_branches: branches.len(),
    })
}

fn text(value: &str, class: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(value));
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    label.add_css_class(class);
    label
}

fn wipe(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

#[derive(Clone)]
pub struct DevelopmentPanel {
    root: gtk::Box,
    state: gtk::Label,
    body: gtk::Box,
    request: Rc<Cell<u64>>,
    context: Rc<RefCell<Option<(String, String, String)>>>,
    window: adw::ApplicationWindow,
    toast: adw::ToastOverlay,
}

impl DevelopmentPanel {
    pub fn new(window: &adw::ApplicationWindow, toast: &adw::ToastOverlay) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 9);
        root.add_css_class("ticket-development");

        let heading = gtk::Box::new(gtk::Orientation::Horizontal, 9);
        let icon = gtk::Image::from_icon_name("folder-symbolic");
        icon.add_css_class("ticket-development-icon");
        heading.append(&icon);
        let title = text("Development", "ticket-development-title");
        title.set_hexpand(true);
        heading.append(&title);
        let state = text("Checking Git…", "ticket-development-badge");
        heading.append(&state);
        let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh.add_css_class("flat");
        refresh.add_css_class("ticket-development-icon-button");
        refresh.set_tooltip_text(Some("Refresh local worktree status"));
        heading.append(&refresh);
        root.append(&heading);

        let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
        root.append(&body);

        let panel = Self {
            root, state, body,
            request: Rc::new(Cell::new(0)),
            context: Rc::new(RefCell::new(None)),
            window: window.clone(),
            toast: toast.clone(),
        };
        let panel_for_refresh = panel.clone();
        refresh.connect_clicked(move |_| {
            let context = panel_for_refresh.context.borrow().clone();
            if let Some((site, key, summary)) = context {
                panel_for_refresh.refresh(&site, &key, &summary);
            }
        });
        panel
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn refresh(&self, site: &str, key: &str, summary: &str) {
        let request = self.request.get().wrapping_add(1);
        self.request.set(request);
        *self.context.borrow_mut() = Some((site.to_owned(), key.to_owned(), summary.to_owned()));
        wipe(&self.body);
        self.set_state("Checking…", None);
        self.body.append(&text("Checking local worktrees…", "ticket-development-muted"));

        let site = site.to_owned();
        let key = key.to_owned();
        let summary = summary.to_owned();
        let worker_site = site.clone();
        let worker_key = key.clone();
        let worker_summary = summary.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = inspect(&worker_site, &worker_key, &worker_summary);
            let _ = sender.send(result);
        });

        let panel = self.clone();
        glib::timeout_add_local(Duration::from_millis(90), move || {
            match receiver.try_recv() {
                Ok(result) => {
                    if panel.request.get() == request {
                        panel.paint(&site, &key, &summary, result);
                    }
                    glib::ControlFlow::Break
                }
                Err(TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(TryRecvError::Disconnected) => {
                    if panel.request.get() == request {
                        panel.paint(&site, &key, &summary,
                            Err("Git status check stopped unexpectedly".to_owned()));
                    }
                    glib::ControlFlow::Break
                },
            }
        });
    }

    fn set_state(&self, value: &str, category: Option<&str>) {
        for class in ["ticket-development-good", "ticket-development-warning"] {
            self.state.remove_css_class(class);
        }
        if let Some(class) = category {
            self.state.add_css_class(class);
        }
        self.state.set_text(value);
    }

    fn info(title: &str, subtitle: &str) -> gtk::Box {
        let info = gtk::Box::new(gtk::Orientation::Vertical, 4);
        info.set_hexpand(true);
        let title_label = text(title, "ticket-development-value");
        title_label.set_tooltip_text(Some(title));
        info.append(&title_label);
        if !subtitle.is_empty() {
            let note = text(subtitle, "ticket-development-muted");
            note.set_tooltip_text(Some(subtitle));
            info.append(&note);
        }
        info
    }

    /// A path is an actual first-class value, not a faint caption. It remains
    /// selectable, can be copied in one click, and exposes its full value.
    fn path_row(&self, path: &str) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.add_css_class("ticket-development-path-row");
        let folder = gtk::Image::from_icon_name("folder-symbolic");
        folder.add_css_class("ticket-development-path-icon");
        row.append(&folder);
        let value = text(path, "ticket-development-path");
        value.set_hexpand(true);
        value.set_selectable(true);
        value.set_tooltip_text(Some(path));
        row.append(&value);
        let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
        copy.add_css_class("flat");
        copy.add_css_class("ticket-development-icon-button");
        copy.set_tooltip_text(Some("Copy worktree path"));
        let destination = path.to_owned();
        let toast = self.toast.clone();
        copy.connect_clicked(move |_| {
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&destination);
                toast.add_toast(adw::Toast::new("Worktree path copied"));
            }
        });
        row.append(&copy);
        row
    }

    fn actions_row(primary: &gtk::Button, secondary: Option<&gtk::Button>) -> gtk::Box {
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.add_css_class("ticket-development-actions");
        actions.append(primary);
        if let Some(secondary) = secondary {
            actions.append(secondary);
        }
        actions
    }

    fn manage_button(&self, site: &str, key: &str, summary: &str) -> gtk::Button {
        self.manager_button("Manage worktrees…", site, key, summary)
    }

    fn manager(&self, site: &str, key: &str, summary: &str) {
        let dialog = git_ui::show(&self.window, site, key, summary);
        let panel = self.clone();
        let site = site.to_owned();
        let key = key.to_owned();
        let summary = summary.to_owned();
        dialog.connect_close_request(move |_| {
            // A newly created/removed worktree must be visible without
            // re-opening the ticket or manually refreshing the board.
            if panel.context.borrow().as_ref().is_some_and(|context|
                context.0 == site && context.1 == key
            ) {
                panel.refresh(&site, &key, &summary);
            }
            glib::Propagation::Proceed
        });
    }

    fn manager_button(&self, label: &str, site: &str, key: &str, summary: &str) -> gtk::Button {
        let button = gtk::Button::with_label(label);
        button.add_css_class("transition-action");
        let panel = self.clone();
        let site = site.to_owned();
        let key = key.to_owned();
        let summary = summary.to_owned();
        button.connect_clicked(move |_| panel.manager(&site, &key, &summary));
        button
    }

    fn editor_button(&self, editor: &str, path: &Path) -> gtk::Button {
        let editor = if editor == "codium" { "codium" } else { "code" };
        let button = gtk::Button::with_label(if editor == "codium" {
            "Open in VSCodium"
        } else {
            "Open in VS Code"
        });
        button.add_css_class("transition-action");
        let path = path.to_path_buf();
        let toast = self.toast.clone();
        button.connect_clicked(move |_| {
            if let Err(error) = git::open_editor(&path, editor) {
                toast.add_toast(adw::Toast::new(&format!("Cannot open worktree: {error}")));
            }
        });
        button
    }

    fn paint(&self, site: &str, key: &str, summary: &str, result: Result<Snapshot, String>) {
        wipe(&self.body);
        match result {
            Ok(Snapshot::Unconfigured) => {
                self.set_state("Not configured", None);
                self.body.append(&Self::info("No Git repositories configured",
                    "Add a local repository to create a ticket worktree."));
                let create = self.manager_button("Configure repositories…", site, key, summary);
                self.body.append(&Self::actions_row(&create, None));
            }
            Ok(Snapshot::Missing {
                repository, branch, destination, base, matching_branches,
            }) => {
                self.set_state("No worktree", None);
                self.body.append(&Self::info(&format!("{repository} · {branch}"), ""));
                self.body.append(&self.path_row(&destination));
                let note = if matching_branches > 1 {
                    "Several matching branches — choose one when creating"
                } else if matching_branches == 1 {
                    "Branch exists — select a starting point before creating"
                } else {
                    "Confirm starting point and setup before creating"
                };
                self.body.append(&text(&format!("{base} · {note}"),
                    "ticket-development-muted"));
                let create = self.manager_button("Create worktree…", site, key, summary);
                self.body.append(&Self::actions_row(&create, None));
            }
            Ok(Snapshot::Existing { editor, worktrees }) => {
                if worktrees.len() == 1 {
                    let tree = &worktrees[0];
                    self.set_state(match tree.dirty {
                        Some(false) => "Clean",
                        Some(true) => "Uncommitted changes",
                        None => "Status unavailable",
                    }, match tree.dirty {
                        Some(false) => Some("ticket-development-good"),
                        Some(true) => Some("ticket-development-warning"),
                        None => None,
                    });
                    self.body.append(&Self::info(
                        &format!("{} · {}", tree.repository, tree.branch), "",
                    ));
                    self.body.append(&self.path_row(&tree.path.to_string_lossy()));
                    let open = self.editor_button(&editor, &tree.path);
                    let manage = self.manage_button(site, key, summary);
                    self.body.append(&Self::actions_row(&open, Some(&manage)));
                } else {
                    self.set_state(&format!("{} worktrees", worktrees.len()), None);
                    for tree in &worktrees {
                        let item = gtk::Box::new(gtk::Orientation::Vertical, 8);
                        item.add_css_class("ticket-development-entry");
                        let status = match tree.dirty {
                            Some(false) => "Clean",
                            Some(true) => "Uncommitted changes",
                            None => "Git status unavailable",
                        };
                        item.append(&Self::info(
                            &format!("{} · {}", tree.repository, tree.branch), status,
                        ));
                        item.append(&self.path_row(&tree.path.to_string_lossy()));
                        let open = self.editor_button(&editor, &tree.path);
                        item.append(&Self::actions_row(&open, None));
                        self.body.append(&item);
                    }
                    let manage = self.manage_button(site, key, summary);
                    self.body.append(&Self::actions_row(&manage, None));
                }
            }
            Err(error) => {
                self.set_state("Unavailable", Some("ticket-development-warning"));
                self.body.append(&Self::info("Git status could not be read", &error));
                let manage = self.manager_button("Manage repositories…", site, key, summary);
                self.body.append(&Self::actions_row(&manage, None));
            }
        }
    }
}
