//! Ticket-focused Git worktree dialog. Git fetch/setup work on background threads.
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::Duration,
};
use adw::prelude::*;
use gtk::glib;
use crate::{
    git::{self, CreateWorktree, Worktree},
    issue_detail,
    preferences::{self, GitRepository, GitSettings},
};

fn label(text: &str) -> gtk::Label {
    let item = gtk::Label::new(Some(text));
    item.set_xalign(0.0);
    item.set_wrap(true);
    item
}

fn field(name: &str, child: &impl IsA<gtk::Widget>) -> gtk::Box {
    let box_ = gtk::Box::new(gtk::Orientation::Vertical, 5);
    let caption = label(name);
    caption.add_css_class("dim-label");
    box_.append(&caption);
    box_.append(child);
    box_
}

fn set_choices(dropdown: &gtk::DropDown, labels: &[String], selected: usize) {
    let values: Vec<&str> = labels.iter().map(String::as_str).collect();
    let model = gtk::StringList::new(&values);
    dropdown.set_model(Some(&model));
    if !labels.is_empty() {
        dropdown.set_selected(selected.min(labels.len() - 1) as u32);
    }
}

fn selected_text(dropdown: &gtk::DropDown) -> Option<String> {
    dropdown.selected_item()
        .and_then(|item| item.downcast::<gtk::StringObject>().ok())
        .map(|item| item.string().to_string())
}

fn selected_index(dropdown: &gtk::DropDown) -> Option<usize> {
    (dropdown.selected() != u32::MAX).then_some(dropdown.selected() as usize)
}

fn report(info: &gtk::Label, message: &str) {
    info.set_text(message);
    info.set_tooltip_text(Some(message));
    info.set_visible(!message.is_empty());
}

fn poll<T: 'static>(receiver: Receiver<T>, done: impl Fn(T) + 'static) {
    glib::timeout_add_local(Duration::from_millis(100), move || {
        match receiver.try_recv() {
            Ok(value) => {
                done(value);
                glib::ControlFlow::Break
            }
            Err(TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(TryRecvError::Disconnected) => glib::ControlFlow::Break,
        }
    });
}

fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

fn suggested_setup(path: &Path) -> &'static str {
    if path.join("pnpm-lock.yaml").exists() { "pnpm install" }
    else if path.join("yarn.lock").exists() { "yarn install" }
    else if path.join("bun.lock").exists() || path.join("bun.lockb").exists() { "bun install" }
    else if path.join("package-lock.json").exists() { "npm install" }
    else { "" }
}

fn register_repository(settings: &Rc<RefCell<GitSettings>>, path: PathBuf) -> Result<(), String> {
    let path = path.canonicalize().map_err(|e| e.to_string())?;
    if !git::is_repository(&path) { return Err("Selected directory is not a Git repository".into()); }
    let name = path.file_name().map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty()).ok_or("Invalid repository name")?;
    if name == "." || name == ".." { return Err("Invalid repository name".into()); }
    let path = path.to_string_lossy().to_string();
    let mut new = settings.borrow().clone();
    if new.repositories.iter().any(|repo| repo.name == name && repo.path != path) {
        return Err(format!("Another registered repository is already called {name}. Rename it first."))
    }
    new.register(GitRepository { name, path, setup: String::new() });
    preferences::save_git(&new).map_err(|e| e.to_string())?;
    *settings.borrow_mut() = new;
    Ok(())
}

struct View {
    dialog: gtk::Window,
    settings: Rc<RefCell<GitSettings>>,
    site: String,
    key: String,
    suggested_branch: String,
    repository: gtk::DropDown,
    base: gtk::DropDown,
    cleanup_target: gtk::DropDown,
    branch: gtk::Entry,
    root: gtk::Entry,
    editor: gtk::DropDown,
    setup: gtk::Entry,
    run_setup: gtk::CheckButton,
    fetch: gtk::CheckButton,
    after_open: gtk::CheckButton,
    summary: gtk::Label,
    path: gtk::Label,
    existing_branch: gtk::Label,
    existing_box: gtk::Box,
    new_box: gtk::Box,
    tree_picker: gtk::DropDown,
    trees: Rc<RefCell<Vec<Worktree>>>,
    more: gtk::MenuButton,
    info: gtk::Label,
    log: gtk::TextView,
    log_scroller: gtk::ScrolledWindow,
    create: gtk::Button,
    open: gtk::Button,
    retry: gtk::Button,
    terminal: gtk::Button,
    remove: gtk::Button,
    active_tree: Rc<RefCell<Option<Worktree>>>,
}

impl View {
    fn show_log(&self, content: &str) {
        self.log.buffer().set_text(content);
        self.log_scroller.set_visible(!content.is_empty());
    }

    fn selected(&self) -> Option<GitRepository> {
        let index = selected_index(&self.repository)?;
        self.settings.borrow().repositories.get(index).cloned()
    }

    fn reload_repositories(&self, preference: Option<usize>) {
        let names: Vec<String> = self.settings.borrow().repositories.iter()
            .map(|repo| repo.name.clone()).collect();
        set_choices(&self.repository, &names, preference.unwrap_or(0));
        if names.is_empty() { self.refresh(); }
    }

    fn refresh(&self) {
        let Some(repo) = self.selected() else {
            self.create.set_sensitive(false);
            self.new_box.set_visible(false);
            self.existing_box.set_visible(false);
            self.open.set_visible(false);
            self.more.set_visible(false);
            self.summary.set_text("Add a local Git repository to get started.");
            self.summary.set_visible(true);
            *self.active_tree.borrow_mut() = None;
            return;
        };
        let path = Path::new(&repo.path);
        let bases = git::available_bases(path).unwrap_or_default();
        let recommended = git::recommended_base(path);
        let index = bases.iter().position(|b| b == &recommended).unwrap_or(0);
        set_choices(&self.base, &bases, index);
        set_choices(&self.cleanup_target, &bases, index);

        let found = git::matching_worktrees(path, &self.key).unwrap_or_default();
        *self.trees.borrow_mut() = found.clone();
        let names: Vec<String> = found.iter().map(|tree|
            tree.branch.as_deref().unwrap_or("Detached HEAD").to_owned()).collect();
        set_choices(&self.tree_picker, &names, 0);
        self.tree_picker.set_visible(found.len() > 1);

        if found.is_empty() {
            *self.active_tree.borrow_mut() = None;
            let existing = git::matching_branches(path, &self.key).unwrap_or_default();
            if let Some(branch) = existing.first() {
                self.branch.set_text(branch);
                self.summary.set_text("Existing branch · a new worktree will check it out");
            } else {
                self.branch.set_text(&self.suggested_branch);
                self.summary.set_text("");
            }
            self.summary.set_visible(!existing.is_empty());
            self.new_box.set_visible(true);
            self.existing_box.set_visible(false);
            self.create.set_visible(true);
            self.create.set_sensitive(true);
            self.open.set_visible(false);
            self.more.set_visible(false);
            self.update_path();
        } else {
            self.choose_tree();
        }
        self.setup.set_text(if repo.setup.is_empty() {
            suggested_setup(path)
        } else { &repo.setup });
        // An auto-detected setup command must be opted into by the user.
        self.run_setup.set_active(!repo.setup.trim().is_empty());
    }

    fn choose_tree(&self) {
        let tree = selected_index(&self.tree_picker)
            .and_then(|index| self.trees.borrow().get(index).cloned())
            .or_else(|| self.trees.borrow().first().cloned());
        *self.active_tree.borrow_mut() = tree.clone();
        let Some(tree) = tree else { return; };
        self.existing_branch.set_text(tree.branch.as_deref().unwrap_or("Detached HEAD"));
        let status = git::status(&tree.path);
        self.summary.remove_css_class("git-status-clean");
        self.summary.remove_css_class("git-status-dirty");
        let message = match status {
            Ok(ref value) if value.is_empty() => {
                self.summary.add_css_class("git-status-clean");
                "Clean worktree"
            }
            Ok(_) => {
                self.summary.add_css_class("git-status-dirty");
                "Uncommitted changes"
            }
            Err(_) => "Git status unavailable",
        };
        self.summary.set_text(message);
        self.summary.set_visible(true);
        self.path.set_text(&tree.path.to_string_lossy());
        self.path.set_tooltip_text(Some(&tree.path.to_string_lossy()));
        self.new_box.set_visible(false);
        self.existing_box.set_visible(true);
        self.create.set_visible(false);
        self.open.set_visible(true);
        self.more.set_visible(true);
    }

    fn update_path(&self) {
        if self.active_tree.borrow().is_some() { return; }
        let Some(repo) = self.selected() else { return; };
        let root = expand_home(&self.root.text());
        let dest = root.join(repo.name).join(&self.key);
        self.path.set_text(&dest.to_string_lossy());
        self.path.set_tooltip_text(Some(&dest.to_string_lossy()));
    }

    fn store_preferences(&self) -> Result<(), String> {
        let mut new = self.settings.borrow().clone();
        new.worktree_root = self.root.text().to_string();
        new.fetch_before_create = self.fetch.is_active();
        new.editor = selected_text(&self.editor)
            .unwrap_or_else(|| "code".to_owned());
        if let Some(index) = selected_index(&self.repository)
            && let Some(repo) = new.repositories.get_mut(index)
        {
            // Skipping setup for one worktree must not erase a saved
            // per-repository command used by later tickets.
            if self.run_setup.is_active() {
                repo.setup = self.setup.text().to_string();
            }
        }
        preferences::save_git(&new).map_err(|e| e.to_string())?;
        *self.settings.borrow_mut() = new;
        Ok(())
    }

    fn create_worktree(self: &Rc<Self>) {
        let Some(repo) = self.selected() else { return; };
        let Some(base) = selected_text(&self.base) else {
            report(&self.info, "Select a base branch"); return;
        };
        let branch = self.branch.text().trim().to_owned();
        if branch.is_empty() { report(&self.info, "Enter a branch name"); return; }
        if !git::branch_matches_issue(&branch, &self.key) {
            report(&self.info, "Branch name must start with the ticket key (optionally after a prefix such as feature/).");
            return;
        }
        if let Err(error) = self.store_preferences() {
            report(&self.info, &error); return;
        }
        let repo_path = repo.path.clone();
        let target = expand_home(&self.root.text()).join(&repo.name).join(&self.key);
        let script = if self.run_setup.is_active() { self.setup.text().to_string() }
            else { String::new() };
        let fetch = self.fetch.is_active();
        let open = self.after_open.is_active();
        let editor = self.settings.borrow().editor.clone();
        self.create.set_sensitive(false);
        report(&self.info, "Creating worktree and running setup…");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let created = git::create(&CreateWorktree {
                repository: Path::new(&repo.path), destination: &target,
                branch: &branch, base: &base, fetch
            });
            let result = match created {
                Ok(()) => {
                    let setup_result = git::run_setup(&target, &script).map_err(|e| e.to_string());
                    (Some(target), setup_result)
                }
                Err(error) => (None, Err(error.to_string())),
            };
            let _ = tx.send(result);
        });
        let view = self.clone();
        poll(rx, move |(path, result)| {
            if let Some(path) = path {
                view.settings.borrow_mut().record_usage(&view.site, &view.key, &repo_path);
                if let Err(error) = preferences::save_git(&view.settings.borrow()) {
                    report(&view.info, &format!("Worktree created; failed to save preference: {error}"));
                }
                view.refresh();
                match result {
                    Ok(log) => {
                        view.show_log(&log);
                        report(&view.info, "Worktree created.");
                        if open {
                            if let Err(error) = git::open_editor(&path, &editor) {
                                report(&view.info, &error.to_string());
                            } else {
                                view.dialog.close();
                            }
                        }
                    }
                    Err(error) => {
                        view.show_log(&error);
                        report(&view.info, "Setup failed. You can retry or open the worktree.");
                    },
                }
            } else if let Err(error) = result {
                report(&view.info, &error);
                view.create.set_sensitive(true);
            }
        });
    }

    fn setup_again(self: &Rc<Self>) {
        let Some(tree) = self.active_tree.borrow().clone() else { return; };
        let script = self.setup.text().to_string();
        if script.trim().is_empty() { report(&self.info, "No setup command configured"); return; }
        if let Err(error) = self.store_preferences() { report(&self.info, &error); return; }
        self.retry.set_sensitive(false);
        report(&self.info, "Running repository setup…");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || { let _ = tx.send(git::run_setup(&tree.path, &script)); });
        let view = self.clone();
        poll(rx, move |result| {
            view.retry.set_sensitive(true);
            match result {
                Ok(log) => {
                    view.show_log(&log);
                    report(&view.info, "Setup completed successfully.");
                }
                Err(error) => {
                    view.show_log(&error.to_string());
                    report(&view.info, "Setup failed. See log.");
                }
            }
        });
    }

    fn confirm_remove(self: &Rc<Self>) {
        let Some(tree) = self.active_tree.borrow().clone() else { return; };
        let Some(repo) = self.selected() else { return; };
        let Some(base) = selected_text(&self.cleanup_target) else { return; };
        let confirmation = gtk::AlertDialog::builder()
            .message("Remove worktree and branch?")
            .detail(format!(
                "Remove {} and its local branch only if clean and provably merged into {}?\n\nIgnored files (including node_modules) may be deleted. Remote branches are never deleted.",
                tree.path.display(), base,
            ))
            .buttons(["Cancel", "Remove"])
            .cancel_button(0)
            .default_button(0)
            .modal(true)
            .build();
        let view = self.clone();
        confirmation.choose(Some(&self.dialog), None::<&gtk::gio::Cancellable>, move |response| {
            if !matches!(response, Ok(1)) { return; }
            view.remove.set_sensitive(false);
            report(&view.info, "Verifying merge and removing worktree…");
            let repo = repo.clone();
            let tree = tree.clone();
            let base = base.clone();
            let key = view.key.clone();
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                let result = git::remove_merged(Path::new(&repo.path), &tree, &base, &key)
                    .map_err(|e| e.to_string());
                let _ = tx.send(result);
            });
            let view = view.clone();
            poll(rx, move |result| {
                view.remove.set_sensitive(true);
                match result {
                    Ok(()) => {
                        view.refresh();
                        report(&view.info, "Worktree and local branch removed.");
                    }
                    Err(error) => report(&view.info, &error),
                }
            });
        });
    }
}

fn open_selected(view: &Rc<View>) {
    let Some(tree) = view.active_tree.borrow().clone() else { return; };
    let editor = selected_text(&view.editor)
        .unwrap_or_else(|| "code".into());
    if let Err(error) = view.store_preferences() {
        report(&view.info, &error); return;
    }
    match git::open_editor(&tree.path, &editor) {
        Ok(()) => view.dialog.close(),
        Err(error) => report(&view.info, &error.to_string()),
    }
}

fn scan_dialog(view: &Rc<View>, root: PathBuf, results: Vec<PathBuf>) {
    if results.is_empty() {
        report(&view.info, "No Git repositories found in the selected folder.");
        return;
    }
    let window = gtk::Window::builder().title("Found repositories")
        .default_width(520).default_height(420).modal(true)
        .transient_for(&view.dialog).build();
    let layout = gtk::Box::new(gtk::Orientation::Vertical, 12);
    layout.set_margin_start(18);
    layout.set_margin_end(18);
    layout.set_margin_top(18);
    layout.set_margin_bottom(18);
    layout.append(&label("Select repositories to register"));
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_vexpand(true);
    let choices = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let mut checks = Vec::new();
    for repo in results {
        let check = gtk::CheckButton::with_label(&repo.to_string_lossy());
        check.set_active(true);
        choices.append(&check);
        checks.push((check, repo));
    }
    scroll.set_child(Some(&choices));
    layout.append(&scroll);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let add = gtk::Button::with_label("Add selected");
    add.add_css_class("suggested-action");
    buttons.append(&cancel);
    buttons.append(&add);
    layout.append(&buttons);
    window.set_child(Some(&layout));
    {
        let window = window.clone();
        cancel.connect_clicked(move |_| window.close());
    }
    {
        let window = window.clone();
        let view = view.clone();
        add.connect_clicked(move |_| {
            let mut errors = Vec::new();
            for (check, repo) in &checks {
                if check.is_active()
                    && let Err(error) = register_repository(&view.settings, repo.clone())
                {
                    errors.push(error);
                }
            }
            let mut settings = view.settings.borrow().clone();
            let root = root.to_string_lossy().to_string();
            if !settings.scan_roots.contains(&root) { settings.scan_roots.push(root); }
            if let Err(error) = preferences::save_git(&settings) {
                errors.push(error.to_string());
            } else {
                *view.settings.borrow_mut() = settings;
            }
            let preference = view.settings.borrow().preferred_index(&view.site, &view.key);
            view.reload_repositories(preference);
            report(&view.info, if errors.is_empty() {
                "Repositories registered."
            } else {
                "Some repositories could not be registered (check duplicate names)."
            });
            window.close();
        });
    }
    window.present();
}

pub fn show(parent: &adw::ApplicationWindow, site: &str, key: &str, summary: &str) {
    let settings = match preferences::load_git() {
        Ok(s) => s,
        Err(error) => {
            let dialog = gtk::Window::builder().title("Git configuration error")
                .transient_for(parent).modal(true).default_width(420).build();
            dialog.set_child(Some(&label(&error.to_string())));
            dialog.present();
            return;
        }
    };
    let dialog = gtk::Window::builder().title(format!("Development · {key}"))
        .transient_for(parent).modal(true).default_width(500)
        .default_height(480).build();
    dialog.add_css_class("git-worktree-dialog");

    // The title and all routine controls stay compact. Rarely used repository
    // registration and setup configuration belong in disclosure menus.
    let surface = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_vexpand(true);
    let main = gtk::Box::new(gtk::Orientation::Vertical, 14);
    main.add_css_class("git-dialog-content");
    scroll.set_child(Some(&main));
    surface.append(&scroll);

    let hero = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let eyebrow = label(&format!("{key} · DEVELOPMENT"));
    eyebrow.add_css_class("git-dialog-eyebrow");
    hero.append(&eyebrow);
    let heading = label(summary);
    heading.add_css_class("git-dialog-title");
    heading.set_wrap(true);
    heading.set_lines(2);
    heading.set_ellipsize(gtk::pango::EllipsizeMode::End);
    hero.append(&heading);
    main.append(&hero);

    let section = gtk::Box::new(gtk::Orientation::Vertical, 12);
    section.add_css_class("git-dialog-section");
    main.append(&section);
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 7);
    let repository = gtk::DropDown::from_strings(&[]);
    repository.set_hexpand(true);
    repository.add_css_class("git-repository-choice");
    let manage = gtk::MenuButton::new();
    manage.set_icon_name("emblem-system-symbolic");
    manage.add_css_class("flat");
    manage.add_css_class("git-small-action");
    manage.set_tooltip_text(Some("Add or scan repositories"));
    let add = gtk::Button::with_label("Add folder…");
    let scan = gtk::Button::with_label("Scan folder…");
    let repo_menu = gtk::Popover::new();
    let repo_actions = gtk::Box::new(gtk::Orientation::Vertical, 4);
    repo_actions.add_css_class("git-menu-actions");
    for button in [&add, &scan] {
        button.set_halign(gtk::Align::Fill);
        button.add_css_class("flat");
        repo_actions.append(button);
    }
    repo_menu.set_child(Some(&repo_actions));
    manage.set_popover(Some(&repo_menu));
    controls.append(&repository);
    controls.append(&manage);
    section.append(&field("Repository", &controls));

    let summary_label = label("");
    summary_label.add_css_class("git-dialog-status");
    summary_label.set_visible(false);
    section.append(&summary_label);

    let new_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let base = gtk::DropDown::from_strings(&[]);
    base.set_hexpand(true);
    new_box.append(&field("Starting point", &base));

    let branch = gtk::Entry::new();
    branch.set_hexpand(true);
    branch.set_text(&issue_detail::suggested_branch_name(key, summary));
    branch.set_tooltip_text(Some(&issue_detail::suggested_branch_name(key, summary)));
    let branch_actions = gtk::Box::new(gtk::Orientation::Horizontal, 7);
    branch_actions.append(&branch);
    let copy_branch = gtk::Button::from_icon_name("edit-copy-symbolic");
    copy_branch.add_css_class("flat");
    copy_branch.add_css_class("git-small-action");
    copy_branch.set_tooltip_text(Some("Copy branch name"));
    branch_actions.append(&copy_branch);
    new_box.append(&field("New branch", &branch_actions));
    {
        let branch = branch.clone();
        copy_branch.connect_clicked(move |_| {
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&branch.text());
            }
        });
    }
    section.append(&new_box);

    // Existing worktrees have their own focused state, never the create form.
    let existing_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    existing_box.set_visible(false);
    let existing_caption = label("EXISTING WORKTREE");
    existing_caption.add_css_class("git-dialog-eyebrow");
    existing_box.append(&existing_caption);
    let tree_picker = gtk::DropDown::from_strings(&[]);
    tree_picker.set_hexpand(true);
    tree_picker.set_visible(false);
    existing_box.append(&tree_picker);
    let existing_branch = label("");
    existing_branch.set_selectable(true);
    existing_branch.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    existing_branch.add_css_class("git-branch-name");
    existing_box.append(&existing_branch);
    let cleanup_target = gtk::DropDown::from_strings(&[]);
    cleanup_target.set_hexpand(true);
    existing_box.append(&field("Merged into (required for safe removal)", &cleanup_target));
    section.append(&existing_box);

    let destination = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    destination.add_css_class("git-dialog-destination");
    let location_icon = gtk::Image::from_icon_name("folder-symbolic");
    location_icon.add_css_class("git-destination-icon");
    destination.append(&location_icon);
    let path = label("");
    path.set_wrap(false);
    path.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    path.set_hexpand(true);
    path.set_selectable(true);
    path.add_css_class("git-dialog-path");
    destination.append(&path);
    let copy_path = gtk::Button::from_icon_name("edit-copy-symbolic");
    copy_path.add_css_class("flat");
    copy_path.add_css_class("git-small-action");
    copy_path.set_tooltip_text(Some("Copy worktree directory"));
    destination.append(&copy_path);
    {
        let path = path.clone();
        copy_path.connect_clicked(move |_| {
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&path.text());
            }
        });
    }
    section.append(&destination);

    let fetch = gtk::CheckButton::with_label("Fetch remote refs before creating");
    fetch.set_active(settings.fetch_before_create);
    new_box.append(&fetch);

    let run_setup = gtk::CheckButton::with_label("Run repository setup once after creating");
    new_box.append(&run_setup);

    let options = gtk::Expander::new(Some("Advanced options"));
    options.add_css_class("git-dialog-options");
    let advanced = gtk::Box::new(gtk::Orientation::Vertical, 12);
    advanced.add_css_class("git-dialog-advanced");
    let setup = gtk::Entry::new();
    setup.set_placeholder_text(Some("e.g. npm install"));
    advanced.append(&field("Setup command", &setup));
    let root = gtk::Entry::new();
    root.set_text(&settings.worktree_root);
    advanced.append(&field("Worktree root (~/Worktrees/<repo>/<ticket>)", &root));
    let editor = gtk::DropDown::from_strings(&["code", "codium"]);
    editor.set_selected(if settings.editor == "codium" { 1 } else { 0 });
    advanced.append(&field("Editor", &editor));
    let after_open = gtk::CheckButton::with_label("Open in editor after creation");
    after_open.set_active(true);
    advanced.append(&after_open);
    options.set_child(Some(&advanced));
    main.append(&options);

    let info = label("");
    info.set_selectable(true);
    info.set_visible(false);
    info.add_css_class("git-dialog-message");
    main.append(&info);
    let log_scroller = gtk::ScrolledWindow::new();
    log_scroller.set_min_content_height(100);
    log_scroller.set_max_content_height(155);
    log_scroller.set_visible(false);
    log_scroller.add_css_class("git-dialog-log");
    let log = gtk::TextView::new();
    log.set_monospace(true);
    log.set_editable(false);
    log.set_cursor_visible(false);
    log.set_wrap_mode(gtk::WrapMode::WordChar);
    log_scroller.set_child(Some(&log));
    main.append(&log_scroller);

    // Footer stays visible when the log or advanced settings grow vertically.
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 9);
    footer.add_css_class("git-dialog-footer");
    let cancel = gtk::Button::with_label("Cancel");
    cancel.add_css_class("flat");
    footer.append(&cancel);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    footer.append(&spacer);
    let more = gtk::MenuButton::new();
    more.set_icon_name("view-more-symbolic");
    more.set_tooltip_text(Some("More worktree actions"));
    more.set_visible(false);
    let actions = gtk::Box::new(gtk::Orientation::Vertical, 4);
    actions.add_css_class("git-menu-actions");
    let retry = gtk::Button::with_label("Run setup again");
    let terminal = gtk::Button::with_label("Open terminal");
    let remove = gtk::Button::with_label("Remove worktree…");
    for action in [&terminal, &retry, &remove] {
        action.add_css_class("flat");
        action.set_halign(gtk::Align::Fill);
        actions.append(action);
    }
    let actions_popover = gtk::Popover::new();
    actions_popover.set_child(Some(&actions));
    more.set_popover(Some(&actions_popover));
    footer.append(&more);
    let open = gtk::Button::with_label("Open in VS Code");
    open.add_css_class("suggested-action");
    let create = gtk::Button::with_label("Create & Open");
    create.add_css_class("suggested-action");
    footer.append(&open);
    footer.append(&create);
    surface.append(&footer);
    dialog.set_child(Some(&surface));
    {
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| dialog.close());
    }
    {
        let create = create.clone();
        after_open.connect_toggled(move |check| {
            create.set_label(if check.is_active() { "Create & Open" } else { "Create worktree" });
        });
    }
    {
        let open = open.clone();
        editor.connect_selected_notify(move |dropdown| {
            open.set_label(if selected_text(dropdown).as_deref() == Some("codium") {
                "Open in VSCodium"
            } else {
                "Open in VS Code"
            });
        });
    }
    let preferred = settings.repositories.iter().position(|repo| {
        git::matching_worktrees(Path::new(&repo.path), key)
            .is_ok_and(|trees| !trees.is_empty())
    }).or_else(|| settings.preferred_index(site, key));
    let view = Rc::new(View {
        dialog: dialog.clone(), settings: Rc::new(RefCell::new(settings)),
        site: site.into(), key: key.into(),
        suggested_branch: issue_detail::suggested_branch_name(key, summary),
        repository: repository.clone(),
        base, cleanup_target, branch, root, editor, setup, run_setup, fetch, after_open,
        summary: summary_label, path, existing_branch, existing_box, new_box,
        tree_picker, trees: Rc::new(RefCell::new(Vec::new())), more,
        info, log, log_scroller, create, open, retry, terminal, remove,
        active_tree: Rc::new(RefCell::new(None)),
    });
    {
        let view = view.clone();
        repository.connect_selected_notify(move |_| view.refresh());
    }
    {
        let view = view.clone();
        view.tree_picker.clone().connect_selected_notify(move |_| view.choose_tree());
    }
    {
        let view = view.clone();
        view.root.connect_changed({
            let view = view.clone();
            move |_| view.update_path()
        });
    }
    {
        let view = view.clone();
        view.create.clone().connect_clicked(move |_| view.create_worktree());
    }
    {
        let view = view.clone();
        view.open.clone().connect_clicked(move |_| open_selected(&view));
    }
    {
        let view = view.clone();
        view.retry.clone().connect_clicked(move |_| view.setup_again());
    }
    {
        let view = view.clone();
        view.remove.clone().connect_clicked(move |_| view.confirm_remove());
    }
    {
        let view = view.clone();
        view.terminal.clone().connect_clicked(move |_| {
            if let Some(tree) = view.active_tree.borrow().clone()
                && let Err(error) = git::open_terminal(&tree.path)
            {
                report(&view.info, &error.to_string());
            }
        });
    }
    {
        let view = view.clone();
        add.connect_clicked(move |_| {
            let chooser = gtk::FileDialog::new();
            let view = view.clone();
            let parent = view.dialog.clone();
            chooser.select_folder(Some(&parent), None::<&gtk::gio::Cancellable>, move |result| {
                if let Ok(file) = result
                    && let Some(path) = file.path()
                {
                    match register_repository(&view.settings, path) {
                        Ok(()) => {
                            let selected = view.settings.borrow().repositories.len() - 1;
                            view.reload_repositories(Some(selected));
                            report(&view.info, "Repository registered.");
                        }
                        Err(error) => report(&view.info, &error),
                    }
                }
            });
        });
    }
    {
        let view = view.clone();
        scan.connect_clicked(move |_| {
            let chooser = gtk::FileDialog::new();
            let view = view.clone();
            let parent = view.dialog.clone();
            chooser.select_folder(Some(&parent), None::<&gtk::gio::Cancellable>, move |result| {
                if let Ok(file) = result
                    && let Some(root) = file.path()
                {
                    report(&view.info, "Scanning selected folder…");
                    let (tx, rx) = mpsc::channel();
                    let root_worker = root.clone();
                    thread::spawn(move || {
                        let _ = tx.send(git::discover(&root_worker, 3));
                    });
                    let view = view.clone();
                    poll(rx, move |results| {
                        report(&view.info, "");
                        scan_dialog(&view, root.clone(), results);
                    });
                }
            });
        });
    }
    view.reload_repositories(preferred);
    dialog.present();
}


/// Header action: open a single known ticket worktree immediately. Open the
/// management dialog only for creation/selection or when multiple exist.
pub fn open_or_manage(
    parent: &adw::ApplicationWindow,
    toast: &adw::ToastOverlay,
    site: &str,
    key: &str,
    summary: &str,
) {
    let parent = parent.clone();
    let toast = toast.clone();
    let site = site.to_owned();
    let key = key.to_owned();
    let summary = summary.to_owned();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = preferences::load_git().map(|settings| {
            let mut found = Vec::new();
            for repo in &settings.repositories {
                if let Ok(trees) = git::matching_worktrees(Path::new(&repo.path), &key) {
                    for tree in trees {
                        found.push(tree.path);
                        if found.len() > 1 { break; }
                    }
                }
                if found.len() > 1 { break; }
            }
            (settings.editor, found)
        });
        let _ = tx.send((site, key, summary, result));
    });
    poll(rx, move |(site, key, summary, result)| {
        if let Ok((editor, trees)) = &result
            && trees.len() == 1
        {
            match git::open_editor(&trees[0], editor) {
                Ok(()) => return,
                Err(error) => {
                    toast.add_toast(adw::Toast::new(&format!("Could not open worktree: {error}")));
                }
            }
        }
        show(&parent, &site, &key, &summary);
    });
}
