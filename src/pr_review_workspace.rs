//! Non-modal PR review workspace. Files are the review units, never directories.
//! A checkbox is a human decision unless the AI has cleared an untouched file.
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::{atomic::{AtomicBool, Ordering}, mpsc::{self, TryRecvError}, Arc},
    thread,
    time::Duration,
};
use adw::prelude::*;
use gtk::glib;
use crate::{
    codex_ui, git_ui, preferences, secrets, pr_review,
    pr_review::{Loaded, PrRepositorySelection, ReviewWorktree},
    pr_review_state::{self, Assessment, ReviewProgress, Verdict},
};

struct WindowHandle {
    window: gtk::Window,
    url: gtk::Entry,
}
thread_local! {
    static PR_WINDOW: RefCell<Option<WindowHandle>> = const { RefCell::new(None) };
}

const FILES_PER_BATCH: usize = 20;

fn label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label
}

fn clear(container: &gtk::Box) {
    while let Some(child) = container.first_child() { container.remove(&child); }
}


fn render_diff(view: &gtk::TextView, source: &str) {
    let buffer = view.buffer();
    let tags = buffer.tag_table();
    if tags.lookup("pr-added").is_none() {
        let dark = adw::StyleManager::default().is_dark();
        for (name, rgba) in [
            ("pr-added", if dark { (0.60, 0.92, 0.68) } else { (0.05, 0.48, 0.22) }),
            ("pr-removed", if dark { (0.98, 0.60, 0.60) } else { (0.68, 0.15, 0.15) }),
            ("pr-hunk", if dark { (0.65, 0.76, 1.00) } else { (0.15, 0.34, 0.72) }),
        ] {
            let tag = gtk::TextTag::new(Some(name));
            tag.set_foreground_rgba(Some(&gtk::gdk::RGBA::new(rgba.0, rgba.1, rgba.2, 1.0)));
            tags.add(&tag);
        }
    }
    buffer.set_text(source);
    let mut cursor = buffer.start_iter();
    for line in source.split_inclusive('\n') {
        let name = if line.starts_with('+') && !line.starts_with("+++") {
            Some("pr-added")
        } else if line.starts_with('-') && !line.starts_with("---") {
            Some("pr-removed")
        } else if line.starts_with("@@") {
            Some("pr-hunk")
        } else { None };
        let begin = cursor;
        let mut end = begin.clone();
        end.forward_chars(line.chars().count() as i32);
        if let Some(name) = name { buffer.apply_tag_by_name(name, &begin, &end); }
        cursor = end;
    }
}

struct FileRow {
    path: String,
    widget: gtk::ListBoxRow,
    check: gtk::CheckButton,
    status: gtk::Label,
}

#[derive(Clone)]
struct Workspace {
    loaded: Rc<RefCell<Option<Loaded>>>,
    state: Rc<RefCell<ReviewProgress>>,
    selected: Rc<RefCell<Option<String>>>,
    rows: Rc<RefCell<Vec<FileRow>>>,
    errors: Rc<RefCell<HashMap<String, String>>>,
    syncing: Rc<Cell<bool>>,
    sequence: Rc<Cell<u64>>,
    running: Rc<Cell<bool>>,
    cancelled: Arc<AtomicBool>,
    list: gtk::ListBox,
    search: gtk::SearchEntry,
    filter: gtk::DropDown,
    diff: gtk::TextView,
    filename: gtk::Label,
    file_info: gtk::Box,
    status: gtk::Label,
    summary: gtk::Label,
    triage: gtk::Button,
    analyze_file: gtk::Button,
    deep: gtk::Button,
    stop: gtk::Button,
    load: gtk::Button,
    worktree_button: gtk::Button,
    worktree_info: gtk::Label,
    worktree_controls: gtk::Box,
    worktree_setup: gtk::Entry,
    worktree_run_setup: gtk::CheckButton,
    setup_output: gtk::Expander,
    setup_output_label: gtk::Label,
    settings: gtk::Expander,
    language: Rc<RefCell<String>>,
    reviewer_notes: Rc<RefCell<String>>,
}

enum ReviewEvent {
    Progress(String),
    File(String, Result<Assessment, String>),
    Finished,
}

impl Workspace {
    fn key(&self) -> Option<String> {
        let loaded = self.loaded.borrow();
        let loaded = loaded.as_ref()?;
        Some(pr_review_state::revision_key(
            &loaded.repository, loaded.pr_number, &loaded.head_ref,
        ))
    }

    fn persist(&self) {
        if let Some(key) = self.key()
            && let Err(error) = pr_review_state::save(&key, &self.state.borrow())
        {
            self.status.set_text(&format!("Cannot save PR review progress: {error}"));
        }
    }

    fn ready(&self) -> bool {
        self.loaded.borrow().as_ref().is_some_and(|loaded|
            loaded.git_error.is_none() && loaded.worktree.as_ref()
                .is_some_and(|tree| tree.ready && tree.path.is_dir()))
    }

    fn set_busy(&self, active: bool) {
        self.running.set(active);
        self.load.set_sensitive(!active);
        self.worktree_button.set_sensitive(!active);
        self.triage.set_sensitive(!active && self.ready());
        let has_selected = self.selected.borrow().is_some();
        self.analyze_file.set_sensitive(!active && self.ready() && has_selected);
        self.deep.set_sensitive(!active && self.ready() && has_selected);
        self.stop.set_sensitive(active);
        // The file list, the diff and manual checkboxes remain usable while AI works.
    }

    fn refresh(&self) {
        self.syncing.set(true);
        let state = self.state.borrow();
        let search = self.search.text().to_lowercase();
        let filter = self.filter.selected();
        for row in self.rows.borrow().iter() {
            let entry = state.entry(&row.path);
            row.check.set_active(entry.checked());
            let category = match entry.ai.as_ref().map(|ai| ai.verdict) {
                Some(Verdict::Routine) => "AI cleared · routine",
                Some(Verdict::Review) => "AI recommends review",
                Some(Verdict::Risk) => "AI flagged risk",
                None => "Not AI-reviewed",
            };
            let human = match entry.human_checked {
                Some(true) => " · checked by you",
                Some(false) => " · pending by your choice",
                None => "",
            };
            let error = self.errors.borrow().get(&row.path).cloned();
            row.status.set_text(&format!("{category}{human}{}", if error.is_some() { " · AI error" } else { "" }));
            if let Some(error) = error {
                row.status.set_tooltip_text(Some(&error));
            } else if let Some(ai) = &entry.ai {
                row.status.set_tooltip_text(Some(&ai.reason));
            }
            let matches_text = search.is_empty() || row.path.to_lowercase().contains(&search);
            let matches_filter = match filter {
                1 => entry.ai.as_ref().is_some_and(|ai|
                    matches!(ai.verdict, Verdict::Review | Verdict::Risk)),
                2 => !entry.checked(),
                3 => entry.ai.as_ref().is_some_and(|ai| ai.verdict == Verdict::Routine),
                _ => true,
            };
            row.widget.set_visible(matches_text && matches_filter);
        }
        self.syncing.set(false);
        let total = self.rows.borrow().len();
        let checked = state.checked_count(self.rows.borrow().iter().map(|row| &row.path));
        let ai_cleared = self.rows.borrow().iter()
            .filter(|row| state.entry(&row.path).ai_cleared()).count();
        let pending_ai = self.rows.borrow().iter()
            .filter(|row| state.entry(&row.path).ai.is_none()).count();
        self.summary.set_text(&format!(
            "{checked}/{total} checked · {ai_cleared} cleared by AI · {pending_ai} not yet AI-reviewed"
        ));
    }

    fn show_file_info(&self, path: &str) {
        clear(&self.file_info);
        let entry = self.state.borrow().entry(path);
        if let Some(ai) = entry.ai {
            let verdict = match ai.verdict {
                Verdict::Routine => "**AI: Routine — no manual review suggested**",
                Verdict::Review => "**AI: Review recommended**",
                Verdict::Risk => "**AI: Potential defect**",
            };
            let text = if ai.impact.is_empty() {
                format!("{verdict}\n\n{}", ai.reason)
            } else {
                format!("{verdict}\n\n{}\n\n**Impact:** {}", ai.reason, ai.impact)
            };
            codex_ui::show_markdown(&self.file_info, &text);
        } else {
            self.file_info.append(&label("Not yet analyzed by Codex. You can inspect the diff now."));
        }
        if let Some(error) = self.errors.borrow().get(path) {
            self.file_info.append(&label(&format!("Codex error: {error}")));
        }
    }

    fn select_file(&self, path: &str) {
        *self.selected.borrow_mut() = Some(path.to_owned());
        self.filename.set_text(path);
        self.show_file_info(path);
        self.analyze_file.set_sensitive(self.ready() && !self.running.get());
        self.deep.set_sensitive(self.ready() && !self.running.get());
        self.sequence.set(self.sequence.get().wrapping_add(1));
        let request_id = self.sequence.get();
        let current_id = self.sequence.clone();
        self.diff.buffer().set_text("Loading Git diff…");
        let Some(loaded) = self.loaded.borrow().clone() else { return };
        if !self.ready() {
            self.diff.buffer().set_text("Create the PR worktree to inspect this file's diff.");
            return;
        }
        let file = path.to_owned();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = pr_review::file_diff(&loaded, &file).map_err(|error| error.to_string());
            let _ = sender.send(result);
        });
        let view = self.diff.clone();
        glib::timeout_add_local(Duration::from_millis(80), move || {
            if current_id.get() != request_id { return glib::ControlFlow::Break; }
            match receiver.try_recv() {
                Ok(Ok(diff)) => {
                    render_diff(&view, if diff.trim().is_empty() {
                        "No textual diff for this path (binary file or rename)."
                    } else { &diff });
                    glib::ControlFlow::Break
                }
                Ok(Err(error)) => {
                    view.buffer().set_text(&format!("Cannot display diff: {error}"));
                    glib::ControlFlow::Break
                }
                Err(TryRecvError::Disconnected) => {
                    view.buffer().set_text("Diff loading stopped.");
                    glib::ControlFlow::Break
                }
                Err(TryRecvError::Empty) => glib::ControlFlow::Continue,
            }
        });
    }

    fn show_files(&self, loaded: Loaded) {
        *self.loaded.borrow_mut() = Some(loaded.clone());
        let key = pr_review_state::revision_key(
            &loaded.repository, loaded.pr_number, &loaded.head_ref,
        );
        match pr_review_state::load(&key) {
            Ok(state) => *self.state.borrow_mut() = state,
            Err(error) => {
                *self.state.borrow_mut() = ReviewProgress::default();
                self.status.set_text(&format!("Review progress could not be loaded: {error}"));
            }
        }
        self.errors.borrow_mut().clear();
        while let Some(child) = self.list.first_child() { self.list.remove(&child); }
        self.rows.borrow_mut().clear();
        *self.selected.borrow_mut() = None;
        let mut paths = loaded.changed_files.clone();
        paths.sort_unstable();
        for path in paths {
            let row = gtk::ListBoxRow::new();
            let horizontal = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            horizontal.set_margin_start(8);
            horizontal.set_margin_end(8);
            horizontal.set_margin_top(6);
            horizontal.set_margin_bottom(6);
            let check = gtk::CheckButton::new();
            check.set_valign(gtk::Align::Center);
            check.set_tooltip_text(Some("Toggle reviewed state (overrides AI recommendation)"));
            horizontal.append(&check);
            let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
            labels.set_hexpand(true);
            let filename = gtk::Label::new(Some(&path));
            filename.set_xalign(0.0);
            filename.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            filename.set_tooltip_text(Some(&path));
            labels.append(&filename);
            let status = label("");
            status.add_css_class("dim-label");
            labels.append(&status);
            horizontal.append(&labels);
            row.set_child(Some(&horizontal));
            self.list.append(&row);
            self.rows.borrow_mut().push(FileRow {
                path: path.clone(), widget: row, check: check.clone(), status,
            });
            let screen = self.clone();
            check.connect_toggled(move |check| {
                if screen.syncing.get() { return; }
                screen.state.borrow_mut().set_human_checked(&path, check.is_active());
                screen.persist();
                screen.refresh();
            });
        }
        if let Some(tree) = loaded.worktree.as_ref() {
            self.worktree_info.set_text(&format!(
                "{} · {}",
                if tree.ready { "PR worktree ready" } else { "PR worktree required" },
                tree.path.display(),
            ));
            self.worktree_button.set_label(if tree.ready { "Run setup again" } else { "Create worktree" });
            self.worktree_setup.set_text(&tree.setup);
            self.worktree_run_setup.set_active(tree.run_setup);
            self.worktree_controls.set_visible(true);
        } else {
            self.worktree_info.set_text(loaded.git_error.as_deref()
                .unwrap_or("No worktree available"));
            self.worktree_controls.set_visible(false);
        }
        self.settings.set_expanded(!loaded.worktree.as_ref().is_some_and(|tree| tree.ready));
        self.refresh();
        self.set_busy(false);
        if let Some(row) = self.rows.borrow().first() {
            self.list.select_row(Some(&row.widget));
        }
        if self.ready() {
            self.status.set_text("Ready. Files and saved review progress restored.");
        } else {
            self.status.set_text("Create the PR worktree to inspect diffs and run Codex.");
        }
    }

    fn analyze(&self, requested: Vec<String>, deep: bool) {
        if self.running.get() || !self.ready() || requested.is_empty() { return; }
        let Some(loaded) = self.loaded.borrow().clone() else { return };
        self.set_busy(true);
        self.cancelled.store(false, Ordering::Relaxed);
        let language = self.language.borrow().clone();
        let notes = self.reviewer_notes.borrow().clone();
        let cancelled = self.cancelled.clone();
        let total = requested.len();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for (batch_index, batch) in requested.chunks(if deep { 1 } else { FILES_PER_BATCH }).enumerate() {
                if cancelled.load(Ordering::Relaxed) { break; }
                let group = batch.to_vec();
                let _ = sender.send(ReviewEvent::Progress(format!(
                    "{} · batch {} · {}/{} files",
                    if deep { "Deep review" } else { "AI triage" },
                    batch_index + 1, batch_index * FILES_PER_BATCH, total,
                )));
                let prompt = if deep {
                    pr_review::deep_prompt(&loaded, &group[0], &language, &notes)
                } else {
                    pr_review::quick_prompt(&loaded, &group, &language, &notes)
                };
                let response = pr_review::run_codex(&loaded, &prompt, !deep, &cancelled, |message| {
                    let _ = sender.send(ReviewEvent::Progress(message));
                });
                let results: Vec<(String, Result<Assessment, String>)> = match response {
                    Ok(response) if deep => vec![(
                        group[0].clone(),
                        pr_review::parse_deep_result(&response).map_err(|error| error.to_string()),
                    )],
                    Ok(response) => pr_review::parse_quick_results(&response, &group)
                        .unwrap_or_else(|error| group.iter().map(|file|
                            (file.clone(), Err(error.to_string()))).collect()),
                    Err(error) => group.iter().map(|file|
                        (file.clone(), Err(error.to_string()))).collect(),
                };
                for (file, result) in results {
                    if sender.send(ReviewEvent::File(file, result)).is_err() { return; }
                }
            }
            let _ = sender.send(ReviewEvent::Finished);
        });
        let screen = self.clone();
        let mut done = 0_usize;
        glib::timeout_add_local(Duration::from_millis(120), move || {
            loop {
                match receiver.try_recv() {
                    Ok(ReviewEvent::Progress(message)) => {
                        screen.status.set_text(&format!(
                            "{done}/{total} files · {}", message.chars().take(180).collect::<String>()
                        ));
                    }
                    Ok(ReviewEvent::File(path, result)) => {
                        done += 1;
                        match result {
                            Ok(assessment) => {
                                screen.state.borrow_mut().set_ai(&path, assessment);
                                screen.errors.borrow_mut().remove(&path);
                                screen.persist();
                            }
                            Err(error) => {
                                screen.errors.borrow_mut().insert(path.clone(), error);
                            }
                        }
                        screen.refresh();
                        if screen.selected.borrow().as_deref() == Some(path.as_str()) {
                            screen.show_file_info(&path);
                        }
                    }
                    Ok(ReviewEvent::Finished) => {
                        let cancelled = screen.cancelled.swap(false, Ordering::Relaxed);
                        screen.set_busy(false);
                        screen.status.set_text(if cancelled {
                            "AI triage stopped. Completed file decisions were saved."
                        } else { "AI triage completed. Review progress saved." });
                        return glib::ControlFlow::Break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        screen.cancelled.store(false, Ordering::Relaxed);
                        screen.set_busy(false);
                        screen.status.set_text("AI process stopped. Completed results remain saved.");
                        return glib::ControlFlow::Break;
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    }

    fn triage_pending(&self) {
        let Some(loaded) = self.loaded.borrow().clone() else { return };
        let state = self.state.borrow();
        let paths = loaded.changed_files.iter()
            .filter(|path| state.entry(path).ai.is_none()).cloned().collect();
        drop(state);
        self.analyze(paths, false);
    }

    fn selected_analysis(&self, deep: bool) {
        let selected = self.selected.borrow().clone();
        if let Some(path) = selected { self.analyze(vec![path], deep); }
    }
}

pub fn show(parent: &impl IsA<gtk::Window>, initial_url: Option<&str>) {
    if PR_WINDOW.with(|slot| {
        if let Some(window) = slot.borrow().as_ref() {
            if let Some(url) = initial_url { window.url.set_text(url); }
            window.window.present();
            true
        } else { false }
    }) { return; }

    let parent: &gtk::Window = parent.as_ref();
    let settings = match preferences::load_git() {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("Cannot load Git settings for PR Review: {error}");
            return;
        }
    };

    let window = gtk::Window::builder()
        .title("PR Review")
        .transient_for(parent)
        .modal(false)
        .default_width((parent.width() * 90 / 100).max(1100))
        .default_height((parent.height() * 90 / 100).max(730))
        .build();
    window.set_hide_on_close(true);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
    root.set_margin_start(12);
    root.set_margin_end(12);
    root.set_margin_top(12);
    root.set_margin_bottom(12);

    let settings_expander = gtk::Expander::new(Some("PR connection and worktree setup"));
    settings_expander.set_expanded(true);
    let form = gtk::Box::new(gtk::Orientation::Vertical, 7);
    let url_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let url = gtk::Entry::new();
    url.set_hexpand(true);
    url.set_placeholder_text(Some("https://dev.azure.com/org/project/_git/repo/pullrequest/123"));
    if let Some(url_value) = initial_url { url.set_text(url_value); }
    let load = gtk::Button::with_label("Load PR");
    load.add_css_class("suggested-action");
    url_row.append(&url);
    url_row.append(&load);
    form.append(&url_row);

    let repository_names: Vec<_> = settings.repositories.iter()
        .map(|repo| format!("{} · {}", repo.name, repo.path)).collect();
    let choices: Vec<_> = repository_names.iter().map(String::as_str).collect();
    let repos = gtk::DropDown::from_strings(&choices);
    repos.set_hexpand(true);
    let lang_choices: Vec<_> = pr_review::RESPONSE_LANGUAGES.iter().map(|(name, _)| *name).collect();
    let lang = gtk::DropDown::from_strings(&lang_choices);
    let selected_lang = pr_review::RESPONSE_LANGUAGES.iter()
        .position(|(_, code)| *code == settings.pr_review_language).unwrap_or(0);
    lang.set_selected(selected_lang as u32);
    let options = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    options.append(&label("Repository"));
    options.append(&repos);
    options.append(&label("AI language"));
    options.append(&lang);
    form.append(&options);

    let pat_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let pat = gtk::PasswordEntry::new();
    pat.set_hexpand(true);
    pat.set_show_peek_icon(true);
    pat.set_placeholder_text(Some("Azure DevOps PAT (optional when saved in keyring)"));
    let remember = gtk::CheckButton::with_label("Remember PAT");
    let forget = gtk::Button::with_label("Forget saved PAT");
    forget.add_css_class("flat");
    pat_row.append(&pat);
    pat_row.append(&remember);
    pat_row.append(&forget);
    form.append(&pat_row);

    let note_input = gtk::TextView::new();
    note_input.set_wrap_mode(gtk::WrapMode::WordChar);
    let note_scroll = gtk::ScrolledWindow::new();
    note_scroll.set_min_content_height(60);
    note_scroll.set_max_content_height(90);
    note_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    note_scroll.set_child(Some(&note_input));
    let notes_expander = gtk::Expander::new(Some("Review context (optional)"));
    notes_expander.set_child(Some(&note_scroll));
    form.append(&notes_expander);

    let worktree_info = label("");
    worktree_info.set_selectable(true);
    form.append(&worktree_info);
    let worktree_controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    worktree_controls.set_visible(false);
    let worktree_run_setup = gtk::CheckButton::with_label("Run setup");
    let worktree_setup = gtk::Entry::new();
    worktree_setup.set_hexpand(true);
    let worktree_button = gtk::Button::with_label("Create worktree");
    worktree_controls.append(&worktree_run_setup);
    worktree_controls.append(&worktree_setup);
    worktree_controls.append(&worktree_button);
    form.append(&worktree_controls);
    let setup_output_label = label("");
    setup_output_label.set_selectable(true);
    let setup_scroll = gtk::ScrolledWindow::new();
    setup_scroll.set_min_content_height(70);
    setup_scroll.set_max_content_height(130);
    setup_scroll.set_child(Some(&setup_output_label));
    let setup_output = gtk::Expander::new(Some("Worktree setup output"));
    setup_output.set_visible(false);
    setup_output.set_child(Some(&setup_scroll));
    form.append(&setup_output);
    settings_expander.set_child(Some(&form));
    root.append(&settings_expander);

    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let triage = gtk::Button::with_label("AI triage pending files");
    triage.add_css_class("suggested-action");
    triage.set_sensitive(false);
    let analyze_file = gtk::Button::with_label("Triage selected");
    analyze_file.set_sensitive(false);
    let deep = gtk::Button::with_label("Deep review selected");
    deep.set_sensitive(false);
    let stop = gtk::Button::with_label("Stop Codex");
    stop.add_css_class("flat");
    stop.set_sensitive(false);
    toolbar.append(&triage);
    toolbar.append(&analyze_file);
    toolbar.append(&deep);
    toolbar.append(&stop);
    root.append(&toolbar);

    let filters = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Find changed file…"));
    search.set_hexpand(true);
    let filter = gtk::DropDown::from_strings(&[
        "All files", "AI recommends review", "Unchecked", "AI cleared"
    ]);
    let summary = label("Load a PR to begin.");
    filters.append(&search);
    filters.append(&filter);
    filters.append(&summary);
    root.append(&filters);

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    paned.set_wide_handle(true);
    paned.set_vexpand(true);
    paned.set_position(450);
    let list_scroll = gtk::ScrolledWindow::new();
    list_scroll.set_min_content_width(340);
    list_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.add_css_class("boxed-list");
    list_scroll.set_child(Some(&list));
    paned.set_start_child(Some(&list_scroll));

    let detail = gtk::Box::new(gtk::Orientation::Vertical, 8);
    detail.set_hexpand(true);
    let filename = label("Select a changed file");
    filename.add_css_class("heading");
    filename.set_selectable(true);
    detail.append(&filename);
    let info_scroller = gtk::ScrolledWindow::new();
    info_scroller.set_min_content_height(80);
    info_scroller.set_max_content_height(180);
    info_scroller.set_size_request(-1, 140);
    info_scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    let file_info = gtk::Box::new(gtk::Orientation::Vertical, 6);
    file_info.set_margin_start(8);
    file_info.set_margin_end(8);
    info_scroller.set_child(Some(&file_info));
    detail.append(&info_scroller);
    let diff = gtk::TextView::new();
    diff.set_monospace(true);
    diff.set_editable(false);
    diff.set_cursor_visible(false);
    diff.set_wrap_mode(gtk::WrapMode::None);
    diff.buffer().set_text("Load a PR to view file diffs.");
    let diff_scroller = gtk::ScrolledWindow::new();
    diff_scroller.set_hexpand(true);
    diff_scroller.set_vexpand(true);
    diff_scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    diff_scroller.set_child(Some(&diff));
    detail.append(&diff_scroller);
    paned.set_end_child(Some(&detail));
    root.append(&paned);

    let status = label("Review progress is saved per PR commit. Closing this window does not stop Codex.");
    status.add_css_class("dim-label");
    root.append(&status);
    window.set_child(Some(&root));

    let screen = Workspace {
        loaded: Rc::new(RefCell::new(None)),
        state: Rc::new(RefCell::new(ReviewProgress::default())),
        selected: Rc::new(RefCell::new(None)),
        rows: Rc::new(RefCell::new(Vec::new())),
        errors: Rc::new(RefCell::new(HashMap::new())),
        syncing: Rc::new(Cell::new(false)),
        sequence: Rc::new(Cell::new(0)),
        running: Rc::new(Cell::new(false)),
        cancelled: Arc::new(AtomicBool::new(false)),
        list: list.clone(), search: search.clone(), filter: filter.clone(),
        diff, filename, file_info, status, summary, triage: triage.clone(),
        analyze_file: analyze_file.clone(), deep: deep.clone(), stop: stop.clone(),
        load: load.clone(), worktree_button: worktree_button.clone(),
        worktree_info, worktree_controls, worktree_setup,
        worktree_run_setup, setup_output, setup_output_label,
        settings: settings_expander,
        language: Rc::new(RefCell::new(pr_review::RESPONSE_LANGUAGES[selected_lang].1.into())),
        reviewer_notes: Rc::new(RefCell::new(String::new())),
    };

    {
        let screen = screen.clone();
        list.connect_row_selected(move |_, row| {
            if let Some(row) = row {
                let index = row.index() as usize;
                let file = screen.rows.borrow().get(index).map(|row| row.path.clone());
                if let Some(file) = file { screen.select_file(&file); }
            }
        });
    }
    {
        let screen = screen.clone();
        search.connect_search_changed(move |_| screen.refresh());
    }
    {
        let screen = screen.clone();
        filter.connect_selected_notify(move |_| screen.refresh());
    }
    {
        let screen = screen.clone();
        triage.connect_clicked(move |_| screen.triage_pending());
    }
    {
        let screen = screen.clone();
        analyze_file.connect_clicked(move |_| screen.selected_analysis(false));
    }
    {
        let screen = screen.clone();
        deep.connect_clicked(move |_| screen.selected_analysis(true));
    }
    {
        let screen = screen.clone();
        stop.connect_clicked(move |_| {
            screen.cancelled.store(true, Ordering::Relaxed);
            screen.stop.set_sensitive(false);
            screen.status.set_text("Stopping Codex; completed file results are already saved…");
        });
    }
    {
        let screen = screen.clone();
        note_input.buffer().connect_changed(move |buffer| {
            *screen.reviewer_notes.borrow_mut() = buffer.text(
                &buffer.start_iter(), &buffer.end_iter(), false,
            ).to_string();
        });
    }
    {
        let screen = screen.clone();
        lang.connect_selected_notify(move |dropdown| {
            if let Some((_, code)) = pr_review::RESPONSE_LANGUAGES.get(dropdown.selected() as usize) {
                *screen.language.borrow_mut() = (*code).to_owned();
                if let Err(error) = preferences::save_pr_review_language(code) {
                    screen.status.set_text(&format!("Cannot save review language: {error}"));
                }
            }
        });
    }
    {
        let screen = screen.clone();
        let url = url.clone();
        let pat = pat.clone();
        forget.connect_clicked(move |button| {
            let address = url.text().to_string();
            let organization = address.split('/').nth(3).unwrap_or("").to_owned();
            if organization.is_empty() {
                screen.status.set_text("Enter an Azure DevOps PR URL first.");
                return;
            }
            button.set_sensitive(false);
            let (sender, receiver) = mpsc::channel();
            thread::spawn(move || {
                let result = secrets::delete_azure_pat(&organization).map_err(|e| e.to_string());
                let _ = sender.send(result);
            });
            let button = button.clone();
            let screen = screen.clone();
            let pat = pat.clone();
            glib::timeout_add_local(Duration::from_millis(100), move || match receiver.try_recv() {
                Ok(Ok(())) => {
                    pat.set_text("");
                    button.set_sensitive(true);
                    screen.status.set_text("Saved PAT forgotten for this organization.");
                    glib::ControlFlow::Break
                }
                Ok(Err(error)) => {
                    button.set_sensitive(true);
                    screen.status.set_text(&format!("Keyring error: {error}"));
                    glib::ControlFlow::Break
                }
                Err(TryRecvError::Disconnected) => {
                    button.set_sensitive(true);
                    glib::ControlFlow::Break
                }
                Err(TryRecvError::Empty) => glib::ControlFlow::Continue,
            });
        });
    }
    {
        let screen = screen.clone();
        let repo_settings = settings.repositories;
        let root = git_ui::expand_home(&settings.worktree_root);
        let url = url.clone();
        let pat = pat.clone();
        let remember = remember.clone();
        load.connect_clicked(move |_| {
            if screen.running.get() { return; }
            let Some(repository) = repo_settings.get(repos.selected() as usize) else {
                screen.status.set_text("Select a local Git repository first.");
                return;
            };
            let configured = !repository.setup.trim().is_empty();
            let setup = if configured {
                repository.setup.clone()
            } else {
                git_ui::suggested_setup(&PathBuf::from(&repository.path)).to_owned()
            };
            let selected = PrRepositorySelection {
                repository: PathBuf::from(&repository.path),
                name: repository.name.clone(),
                root: root.clone(),
                setup,
                run_setup: configured,
            };
            let address = url.text().to_string();
            let token = pat.text().to_string();
            let remember_token = remember.is_active();
            screen.set_busy(true);
            screen.status.set_text("Loading Azure PR and checking Git references…");
            let (sender, receiver) = mpsc::channel();
            thread::spawn(move || {
                let result = pr_review::load_pr_with_credentials(
                    &address, &token, remember_token, selected,
                ).map_err(|error| error.to_string());
                let _ = sender.send(result);
            });
            let screen = screen.clone();
            glib::timeout_add_local(Duration::from_millis(100), move || {
                match receiver.try_recv() {
                    Ok(Ok((loaded, _source))) => {
                        screen.show_files(loaded);
                        if screen.ready() { screen.triage_pending(); }
                        glib::ControlFlow::Break
                    }
                    Ok(Err(error)) => {
                        screen.set_busy(false);
                        screen.status.set_text(&format!("Cannot load PR: {error}"));
                        glib::ControlFlow::Break
                    }
                    Err(TryRecvError::Disconnected) => {
                        screen.set_busy(false);
                        screen.status.set_text("PR loading interrupted.");
                        glib::ControlFlow::Break
                    }
                    Err(TryRecvError::Empty) => glib::ControlFlow::Continue,
                }
            });
        });
    }
    {
        let screen = screen.clone();
        worktree_button.connect_clicked(move |_| {
            if screen.running.get() { return; }
            let Some(loaded) = screen.loaded.borrow().clone() else { return };
            let Some(mut tree) = loaded.worktree.clone() else { return };
            tree.setup = screen.worktree_setup.text().to_string();
            tree.run_setup = screen.worktree_run_setup.is_active();
            if tree.ready && (!tree.run_setup || tree.setup.trim().is_empty()) {
                screen.status.set_text("Enable the setup command before running it again.");
                return;
            }
            screen.set_busy(true);
            screen.status.set_text("Creating PR worktree and running configured setup…");
            let (sender, receiver) = mpsc::channel();
            let repository_path = loaded.repository.clone();
            thread::spawn(move || {
                let result = pr_review::create_pr_worktree(&loaded, &tree)
                    .map_err(|error| error.to_string());
                let _ = sender.send((tree, result));
            });
            let screen = screen.clone();
            glib::timeout_add_local(Duration::from_millis(100), move || {
                match receiver.try_recv() {
                    Ok((tree, Ok((output, _created)))) => {
                        if let Some(loaded) = screen.loaded.borrow_mut().as_mut() {
                            loaded.worktree = Some(ReviewWorktree { ready: true, ..tree.clone() });
                        }
                        screen.worktree_info.set_text(&format!("PR worktree ready · {}", tree.path.display()));
                        screen.worktree_button.set_label("Run setup again");
                        screen.settings.set_expanded(false);
                        if tree.run_setup && !tree.setup.trim().is_empty() {
                            match preferences::load_git() {
                                Ok(mut settings) => {
                                    if let Some(repo) = settings.repositories.iter_mut()
                                        .find(|repo| repo.path == repository_path.to_string_lossy().as_ref())
                                    {
                                        repo.setup = tree.setup.clone();
                                        if let Err(error) = preferences::save_git(&settings) {
                                            screen.status.set_text(&format!(
                                                "Setup ran, but preference could not be saved: {error}"
                                            ));
                                        }
                                    }
                                }
                                Err(error) => screen.status.set_text(&format!(
                                    "Cannot read setup preferences: {error}"
                                )),
                            }
                        }
                        if !output.is_empty() {
                            screen.setup_output_label.set_text(&output);
                            screen.setup_output.set_visible(true);
                        }
                        screen.set_busy(false);
                        if let Some(file) = screen.selected.borrow().clone() {
                            screen.select_file(&file);
                        }
                        screen.triage_pending();
                        glib::ControlFlow::Break
                    }
                    Ok((_tree, Err(error))) => {
                        screen.set_busy(false);
                        screen.setup_output_label.set_text(&error);
                        screen.setup_output.set_visible(true);
                        screen.setup_output.set_expanded(true);
                        screen.status.set_text("Worktree or setup failed; inspect output and retry.");
                        glib::ControlFlow::Break
                    }
                    Err(TryRecvError::Disconnected) => {
                        screen.set_busy(false);
                        screen.status.set_text("Worktree setup interrupted.");
                        glib::ControlFlow::Break
                    }
                    Err(TryRecvError::Empty) => glib::ControlFlow::Continue,
                }
            });
        });
    }

    PR_WINDOW.with(|slot| *slot.borrow_mut() = Some(WindowHandle {
        window: window.clone(),
        url: url.clone(),
    }));
    window.present();
}
