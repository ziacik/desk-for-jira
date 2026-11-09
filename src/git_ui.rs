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

fn report(info: &gtk::Label, message: &str) {
    info.set_text(message);
    info.set_tooltip_text(Some(message));
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
    repository: gtk::ComboBoxText,
    base: gtk::ComboBoxText,
    branch: gtk::Entry,
    root: gtk::Entry,
    editor: gtk::ComboBoxText,
    setup: gtk::Entry,
    run_setup: gtk::CheckButton,
    fetch: gtk::CheckButton,
    after_open: gtk::CheckButton,
    summary: gtk::Label,
    path: gtk::Label,
    info: gtk::Label,
    log: gtk::TextView,
    log_scroller: gtk::ScrolledWindow,
    create: gtk::Button,
    open: gtk::Button,
    retry: gtk::Button,
    remove: gtk::Button,
    active_tree: Rc<RefCell<Option<Worktree>>>,
}

impl View {
    fn show_log(&self, content: &str) {
        self.log.buffer().set_text(content);
        self.log_scroller.set_visible(!content.is_empty());
    }

    fn selected(&self) -> Option<GitRepository> {
        let index = self.repository.active()? as usize;
        self.settings.borrow().repositories.get(index).cloned()
    }

    fn reload_repositories(&self, preference: Option<usize>) {
        self.repository.remove_all();
        let repos = &self.settings.borrow().repositories;
        for repo in repos {
            self.repository.append_text(&repo.name);
        }
        if !repos.is_empty() {
            self.repository.set_active(Some(preference.unwrap_or(0).min(repos.len() - 1) as u32));
        } else {
            self.refresh();
        }
    }

    fn refresh(&self) {
        let Some(repo) = self.selected() else {
            self.create.set_sensitive(false);
            self.open.set_visible(false);
            self.retry.set_visible(false);
            self.remove.set_visible(false);
            self.summary.set_text("Register a local repository to begin.");
            *self.active_tree.borrow_mut() = None;
            return;
        };
        let path = Path::new(&repo.path);
        self.base.remove_all();
        let bases = git::available_bases(path).unwrap_or_default();
        let recommended = git::recommended_base(path);
        let index = bases.iter().position(|b| b == &recommended).unwrap_or(0);
        for base in &bases { self.base.append_text(base); }
        if !bases.is_empty() { self.base.set_active(Some(index as u32)); }

        let trees = git::matching_worktrees(path, &self.key).unwrap_or_default();
        let tree = trees.first().cloned();
        *self.active_tree.borrow_mut() = tree.clone();
        if let Some(tree) = tree {
            self.branch.set_text(tree.branch.as_deref().unwrap_or(""));
            let dirty = git::status(&tree.path).map(|s| !s.is_empty());
            self.summary.set_text(match dirty {
                Ok(true) => "Existing worktree · uncommitted changes",
                Ok(false) => "Existing worktree · clean",
                Err(_) => "Existing worktree · Git status unavailable",
            });
            self.path.set_text(&tree.path.to_string_lossy());
            self.create.set_visible(false);
            self.open.set_visible(true);
            self.retry.set_visible(true);
            self.remove.set_visible(true);
        } else {
            let existing = git::matching_branches(path, &self.key).unwrap_or_default();
            if let Some(branch) = existing.first() {
                self.branch.set_text(branch);
                self.summary.set_text("Existing branch · a new worktree will check it out");
            } else {
                self.branch.set_text(&self.suggested_branch);
                self.summary.set_text("New worktree and branch");
            }
            self.create.set_visible(true);
            self.create.set_sensitive(true);
            self.open.set_visible(false);
            self.retry.set_visible(false);
            self.remove.set_visible(false);
            self.update_path();
        }
        self.setup.set_text(if repo.setup.is_empty() {
            suggested_setup(path)
        } else { &repo.setup });
        // Auto-suggested scripts require explicit opt-in the first time.
        self.run_setup.set_active(!repo.setup.trim().is_empty());
    }

    fn update_path(&self) {
        if self.active_tree.borrow().is_some() { return; }
        let Some(repo) = self.selected() else { return; };
        let root = expand_home(&self.root.text());
        let dest = root.join(repo.name).join(&self.key);
        self.path.set_text(&dest.to_string_lossy());
    }

    fn store_preferences(&self) -> Result<(), String> {
        let mut new = self.settings.borrow().clone();
        new.worktree_root = self.root.text().to_string();
        new.fetch_before_create = self.fetch.is_active();
        new.editor = self.editor.active_id().map(|s| s.to_string())
            .unwrap_or_else(|| "code".to_owned());
        if let Some(index) = self.repository.active().map(|i| i as usize)
            && let Some(repo) = new.repositories.get_mut(index)
        {
            repo.setup = if self.run_setup.is_active() {
                self.setup.text().to_string()
            } else { String::new() };
        }
        preferences::save_git(&new).map_err(|e| e.to_string())?;
        *self.settings.borrow_mut() = new;
        Ok(())
    }

    fn create_worktree(self: &Rc<Self>) {
        let Some(repo) = self.selected() else { return; };
        let Some(base) = self.base.active_text().map(|s| s.to_string()) else {
            report(&self.info, "Select a base branch"); return;
        };
        let branch = self.branch.text().trim().to_owned();
        if branch.is_empty() { report(&self.info, "Enter a branch name"); return; }
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
        let Some(base) = self.base.active_text().map(|s| s.to_string()) else { return; };
        let confirmation = gtk::Dialog::new();
        confirmation.set_title(Some("Remove worktree and branch?"));
        confirmation.set_transient_for(Some(&self.dialog));
        confirmation.set_modal(true);
        confirmation.add_button("Cancel", gtk::ResponseType::Cancel);
        confirmation.add_button("Remove", gtk::ResponseType::Accept);
        let content = label(&format!(
            "Remove {} and its local branch only if clean and provably merged into {}?\n\nIgnored files (including node_modules) may be deleted. Remote branches are never deleted.",
            tree.path.display(), base,
        ));
        content.set_margin_start(16);
        content.set_margin_end(16);
        content.set_margin_top(16);
        content.set_margin_bottom(16);
        confirmation.content_area().append(&content);
        let view = self.clone();
        confirmation.connect_response(move |dialog, response| {
            dialog.close();
            if response != gtk::ResponseType::Accept { return; }
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
        confirmation.present();
    }
}

fn open_selected(view: &Rc<View>) {
    let Some(tree) = view.active_tree.borrow().clone() else { return; };
    let editor = view.editor.active_id().map(|s| s.to_string())
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
        .transient_for(parent).modal(true).default_width(540)
        .default_height(580).build();
    let main = gtk::Box::new(gtk::Orientation::Vertical, 12);
    main.set_margin_start(20);
    main.set_margin_end(20);
    main.set_margin_top(18);
    main.set_margin_bottom(18);
    let heading = label(summary);
    heading.add_css_class("title-4");
    main.append(&heading);

    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let repository = gtk::ComboBoxText::new();
    repository.set_hexpand(true);
    let add = gtk::Button::with_label("Add folder");
    let scan = gtk::Button::with_label("Scan…");
    controls.append(&repository);
    controls.append(&add);
    controls.append(&scan);
    main.append(&field("Repository", &controls));

    let base = gtk::ComboBoxText::new();
    base.set_hexpand(true);
    main.append(&field("Base branch (also used for safe cleanup)", &base));
    let branch = gtk::Entry::new();
    branch.set_text(&issue_detail::suggested_branch_name(key, summary));
    main.append(&field("Branch", &branch));

    let root = gtk::Entry::new();
    root.set_text(&settings.worktree_root);
    main.append(&field("Worktree root (~/Worktrees/<repo>/<ticket>)", &root));
    let path = label("");
    path.set_selectable(true);
    path.add_css_class("dim-label");
    main.append(&path);
    let fetch = gtk::CheckButton::with_label("Fetch remote refs before creating");
    fetch.set_active(settings.fetch_before_create);
    main.append(&fetch);

    let setup = gtk::Entry::new();
    setup.set_placeholder_text(Some("npm install / pnpm install / other command"));
    main.append(&field("Setup command (runs once after creation)", &setup));
    let run_setup = gtk::CheckButton::with_label("Run setup after creating");
    main.append(&run_setup);
    let editor = gtk::ComboBoxText::new();
    editor.append(Some("code"), "VS Code");
    editor.append(Some("codium"), "VSCodium");
    editor.set_active_id(Some(if settings.editor == "codium" { "codium" } else { "code" }));
    main.append(&field("Editor", &editor));
    let after_open = gtk::CheckButton::with_label("Open in editor after creation");
    after_open.set_active(true);
    main.append(&after_open);

    let summary_label = label("");
    summary_label.add_css_class("dim-label");
    main.append(&summary_label);
    let info = label("");
    info.set_selectable(true);
    info.set_wrap(true);
    main.append(&info);
    let log_scroller = gtk::ScrolledWindow::new();
    log_scroller.set_min_content_height(90);
    log_scroller.set_max_content_height(160);
    log_scroller.set_visible(false);
    let log = gtk::TextView::new();
    log.set_monospace(true);
    log.set_editable(false);
    log.set_cursor_visible(false);
    log.set_wrap_mode(gtk::WrapMode::WordChar);
    log_scroller.set_child(Some(&log));
    main.append(&log_scroller);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    let create = gtk::Button::with_label("Create & Open");
    create.add_css_class("suggested-action");
    let open = gtk::Button::with_label("Open in editor");
    open.add_css_class("suggested-action");
    let retry = gtk::Button::with_label("Run setup");
    let remove = gtk::Button::with_label("Remove…");
    actions.append(&remove);
    actions.append(&retry);
    actions.append(&open);
    actions.append(&create);
    main.append(&actions);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&main));
    dialog.set_child(Some(&scroll));

    let preferred = settings.repositories.iter().position(|repo| {
        git::matching_worktrees(Path::new(&repo.path), key)
            .is_ok_and(|trees| !trees.is_empty())
    }).or_else(|| settings.preferred_index(site, key));
    let view = Rc::new(View {
        dialog: dialog.clone(), settings: Rc::new(RefCell::new(settings)),
        site: site.into(), key: key.into(),
        suggested_branch: issue_detail::suggested_branch_name(key, summary),
        repository: repository.clone(),
        base, branch, root, editor, setup, run_setup, fetch, after_open,
        summary: summary_label, path, info, log, log_scroller, create, open, retry, remove,
        active_tree: Rc::new(RefCell::new(None)),
    });
    {
        let view = view.clone();
        repository.connect_changed(move |_| view.refresh());
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
        add.connect_clicked(move |_| {
            let chooser = gtk::FileDialog::new();
            let view = view.clone();
            chooser.select_folder(Some(&view.dialog), None::<&gtk::gio::Cancellable>, move |result| {
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
            chooser.select_folder(Some(&view.dialog), None::<&gtk::gio::Cancellable>, move |result| {
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
                        scan_dialog(&view, root, results);
                    });
                }
            });
        });
    }
    view.reload_repositories(preferred);
    dialog.present();
}
