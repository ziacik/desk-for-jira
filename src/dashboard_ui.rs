use std::{cell::{Cell, RefCell}, rc::Rc, sync::mpsc, thread, time::Duration};
use adw::prelude::*;
use gtk::glib;
use crate::{dashboards::{self, Dashboard, Gadget, Release}, jira::{Credentials, Issue, JiraClient}, preferences};

const PAGE_NAME: &str = "jira-dashboards";
type OpenIssue = Rc<dyn Fn(&str)>;

fn background<T: Send + 'static>(
    job: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
    done: impl FnOnce(Result<T, String>) + 'static,
) {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || { let _ = sender.send(job().map_err(|e| e.to_string())); });
    let mut done = Some(done);
    glib::timeout_add_local(Duration::from_millis(80), move || {
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => Err("Dashboard request stopped unexpectedly".to_owned()),
        };
        if let Some(done) = done.take() { done(result); }
        glib::ControlFlow::Break
    });
}

fn clear(container: &gtk::Box) {
    while let Some(child) = container.first_child() { container.remove(&child); }
}

fn label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label
}

fn heading(text: &str) -> gtk::Label {
    let label = label(text);
    label.add_css_class("title-3");
    label
}

fn note(container: &gtk::Box, text: &str) {
    let message = label(text);
    message.add_css_class("dim-label");
    container.append(&message);
}

fn issues(container: &gtk::Box, items: &[Issue], open: &OpenIssue) {
    if items.is_empty() { note(container, "No matching issues."); return; }
    let suffix = if items.len() >= 500 { " (first 500)" } else { "" };
    note(container, &format!("{} issues{suffix}", items.len()));
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");
    for issue in items {
        let row = adw::ActionRow::builder()
            .title(format!("{} — {}", issue.key, issue.summary))
            .subtitle(format!("{} · {} · {}", issue.status, issue.assignee, issue.fix_versions.join(", ")))
            .activatable(true)
            .build();
        row.set_use_markup(false);
        let key = issue.key.clone();
        let open = open.clone();
        row.connect_activated(move |_| open(&key));
        list.append(&row);
    }
    container.append(&list);
}

#[derive(Clone)]
struct View {
    account: Credentials,
    projects: Vec<String>,
    saved: Rc<RefCell<Vec<Dashboard>>>,
    selection: Rc<RefCell<Option<String>>>,
    request: Rc<Cell<u64>>,
    saved_box: glib::WeakRef<gtk::Box>,
    body: glib::WeakRef<gtk::Box>,
    message: glib::WeakRef<gtk::Label>,
    open_issue: OpenIssue,
}

impl View {
    fn message(&self, message: &str) {
        if let Some(label) = self.message.upgrade() { label.set_text(message); }
    }

    fn save(&self, dashboards: Vec<Dashboard>) -> bool {
        match preferences::save_dashboards(&self.account.base_url, &dashboards) {
            Ok(()) => {
                *self.saved.borrow_mut() = dashboards;
                self.message("");
                true
            }
            Err(error) => {
                self.message(&format!("Could not save dashboards: {error}"));
                false
            }
        }
    }

    fn add(&self, dashboard: Dashboard) {
        let mut saved = self.saved.borrow().clone();
        if !saved.iter().any(|d| d.id == dashboard.id) {
            saved.push(dashboard.clone());
            if !self.save(saved) { return; }
        }
        self.select(&dashboard.id);
    }

    fn render_saved(&self) {
        let Some(container) = self.saved_box.upgrade() else { return; };
        clear(&container);
        let selected = self.selection.borrow().clone();
        for dashboard in self.saved.borrow().iter() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            let button = gtk::Button::with_label(&dashboard.name);
            button.set_hexpand(true);
            button.add_css_class(if selected.as_deref() == Some(dashboard.id.as_str()) { "suggested-action" } else { "flat" });
            let remove = gtk::Button::from_icon_name("list-remove-symbolic");
            remove.add_css_class("flat");
            remove.set_tooltip_text(Some("Remove saved dashboard (locally only)"));
            {
                let view = self.clone();
                let id = dashboard.id.clone();
                button.connect_clicked(move |_| view.select(&id));
            }
            {
                let view = self.clone();
                let id = dashboard.id.clone();
                remove.connect_clicked(move |_| {
                    let mut saved = view.saved.borrow().clone();
                    saved.retain(|d| d.id != id);
                    if view.save(saved) {
                        if view.selection.borrow().as_deref() == Some(id.as_str()) {
                            view.selection.borrow_mut().take();
                            view.request.set(view.request.get().wrapping_add(1));
                            if let Some(body) = view.body.upgrade() {
                                clear(&body);
                                note(&body, "Choose or add a dashboard.");
                            }
                        }
                        view.render_saved();
                    }
                });
            }
            row.append(&button);
            row.append(&remove);
            container.append(&row);
        }
        if self.saved.borrow().is_empty() { note(&container, "No saved dashboards."); }
    }

    fn select(&self, id: &str) {
        let Some(dashboard) = self.saved.borrow().iter().find(|d| d.id == id).cloned() else { return; };
        let Some(body) = self.body.upgrade() else { return; };
        *self.selection.borrow_mut() = Some(id.to_owned());
        self.render_saved();
        self.request.set(self.request.get().wrapping_add(1));
        let generation = self.request.get();
        clear(&body);
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let title = heading(&dashboard.name);
        title.set_hexpand(true);
        header.append(&title);
        let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh.set_tooltip_text(Some("Refresh dashboard"));
        let url = format!("{}/jira/dashboards/{}", self.account.base_url, dashboard.id);
        let browser = gtk::LinkButton::with_label(&url, "Open in Jira");
        header.append(&refresh);
        header.append(&browser);
        body.append(&header);
        {
            let view = self.clone();
            let id = dashboard.id.clone();
            refresh.connect_clicked(move |_| view.select(&id));
        }

        body.append(&heading("Upcoming releases"));
        note(&body, "Local release view: select a Jira project to see unreleased versions and their assigned bugs.");
        let mut names = vec!["Select a project…"];
        names.extend(self.projects.iter().map(String::as_str));
        let project = gtk::DropDown::from_strings(&names);
        let selected = self.projects.iter().position(|p| p == &dashboard.project).map_or(0, |i| i as u32 + 1);
        project.set_selected(selected);
        body.append(&project);
        let release_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
        body.append(&release_box);
        if selected > 0 { load_releases(&release_box, &self.account, &dashboard.project, &self.open_issue); }
        {
            let view = self.clone();
            let id = dashboard.id.clone();
            let release_box = release_box.downgrade();
            project.connect_selected_notify(move |dropdown| {
                let Some(release_box) = release_box.upgrade() else { return; };
                let key = dropdown.selected().checked_sub(1)
                    .and_then(|i| view.projects.get(i as usize)).cloned().unwrap_or_default();
                let mut saved = view.saved.borrow().clone();
                if let Some(d) = saved.iter_mut().find(|d| d.id == id) { d.project = key.clone(); }
                if view.save(saved) { view.select(&id); }
                else { clear(&release_box); note(&release_box, "Project selection was not saved."); }
            });
        }

        body.append(&heading("Dashboard gadgets"));
        let gadgets = gtk::Box::new(gtk::Orientation::Vertical, 10);
        body.append(&gadgets);
        note(&gadgets, "Loading Jira gadgets…");
        let account = self.account.clone();
        let id = dashboard.id;
        let view = self.clone();
        let gadgets = gadgets.downgrade();
        background(move || dashboards::gadgets(&JiraClient::new(account)?, &id), move |result| {
            if view.request.get() != generation { return; }
            let Some(container) = gadgets.upgrade() else { return; };
            clear(&container);
            match result {
                Ok(items) if items.is_empty() => note(&container, "No dashboard gadgets returned by Jira."),
                Ok(items) => for Gadget { title, data } in items {
                    let group = gtk::Box::new(gtk::Orientation::Vertical, 6);
                    group.append(&heading(&title));
                    match data {
                        Ok(items) => issues(&group, &items, &view.open_issue),
                        Err(error) => note(&group, &error),
                    }
                    container.append(&group);
                },
                Err(error) => note(&container, &format!("Could not load gadgets: {error}")),
            }
        });
    }
}

fn load_releases(container: &gtk::Box, account: &Credentials, project: &str, open: &OpenIssue) {
    clear(container);
    note(container, "Loading releases…");
    let account = account.clone();
    let project = project.to_owned();
    let container = container.downgrade();
    let worker_account = account.clone();
    let open = open.clone();
    background(move || dashboards::releases(&JiraClient::new(worker_account)?, &project), move |result| {
        let Some(container) = container.upgrade() else { return; };
        clear(&container);
        let releases = match result {
            Ok(releases) => releases,
            Err(error) => { note(&container, &format!("Could not load releases: {error}")); return; }
        };
        if releases.is_empty() { note(&container, "No unreleased versions."); return; }
        let results = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let request = Rc::new(Cell::new(0u64));
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::None);
        for Release { id, name, date } in releases {
            let row = adw::ActionRow::builder()
                .title(&name)
                .subtitle(if date.is_empty() { "No release date" } else { &date })
                .activatable(true)
                .build();
            row.set_use_markup(false);
            let arrow = gtk::Image::from_icon_name("go-next-symbolic");
            row.add_suffix(&arrow);
            let account = account.clone();
            let results = results.downgrade();
            let request = request.clone();
            let open = open.clone();
            row.connect_activated(move |_| {
                let Some(results) = results.upgrade() else { return; };
                clear(&results);
                results.append(&heading(&format!("Bugs — {name}")));
                note(&results, "Loading bugs…");
                request.set(request.get().wrapping_add(1));
                let generation = request.get();
                let request = request.clone();
                let account = account.clone();
                let id = id.clone();
                let name = name.clone();
                let open = open.clone();
                let results = results.downgrade();
                background(move || dashboards::release_bugs(&JiraClient::new(account)?, &id), move |result| {
                    if request.get() != generation { return; }
                    let Some(results) = results.upgrade() else { return; };
                    clear(&results);
                    results.append(&heading(&format!("Bugs — {name}")));
                    match result {
                        Ok(items) => issues(&results, &items, &open),
                        Err(error) => note(&results, &format!("Could not load bugs: {error}")),
                    }
                });
            });
            list.append(&row);
        }
        container.append(&list);
        container.append(&results);
    });
}

fn existing(tabs: &adw::TabView) -> Option<adw::TabPage> {
    (0..tabs.n_pages()).map(|i| tabs.nth_page(i))
        .find(|page| page.child().widget_name() == PAGE_NAME)
}

pub fn close(tabs: &adw::TabView) {
    if let Some(page) = existing(tabs) { tabs.close_page(&page); }
}

pub fn show(tabs: &adw::TabView, account: Credentials, mut projects: Vec<String>, open_issue: OpenIssue) {
    if let Some(page) = existing(tabs) {
        tabs.set_selected_page(&page);
        return;
    }
    projects.sort();
    projects.dedup();
    let root = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    root.set_widget_name(PAGE_NAME);
    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 10);
    sidebar.set_width_request(240);
    sidebar.set_margin_top(16);
    sidebar.set_margin_start(16);
    sidebar.set_margin_bottom(16);
    sidebar.append(&heading("Saved dashboards"));
    let saved_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    sidebar.append(&saved_box);
    sidebar.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    sidebar.append(&heading("Add dashboard"));
    let chooser = gtk::DropDown::from_strings(&["Loading dashboards…"]);
    chooser.set_sensitive(false);
    sidebar.append(&chooser);
    let add = gtk::Button::with_label("Add selected dashboard");
    add.set_sensitive(false);
    sidebar.append(&add);
    let entry = gtk::Entry::builder().placeholder_text("Dashboard URL or ID").build();
    sidebar.append(&entry);
    let add_url = gtk::Button::with_label("Add from URL or ID");
    sidebar.append(&add_url);
    let message = label("");
    sidebar.append(&message);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 14);
    body.set_margin_top(16);
    body.set_margin_end(16);
    body.set_margin_bottom(16);
    body.set_hexpand(true);
    let scroll = gtk::ScrolledWindow::builder().hexpand(true).vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never).child(&body).build();
    let sidebar_scroll = gtk::ScrolledWindow::builder().vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never).child(&sidebar).build();
    root.append(&sidebar_scroll);
    root.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    root.append(&scroll);
    let page = tabs.append(&root);
    page.set_title("Dashboards");
    tabs.set_selected_page(&page);

    let saved = match preferences::load_dashboards(&account.base_url) {
        Ok(saved) => saved,
        Err(error) => {
            note(&body, &format!("Could not load saved dashboards: {error}"));
            sidebar.set_sensitive(false);
            return;
        }
    };
    let view = View {
        account, projects, saved: Rc::new(RefCell::new(saved)), selection: Rc::new(RefCell::new(None)),
        request: Rc::new(Cell::new(0)), saved_box: saved_box.downgrade(), body: body.downgrade(),
        message: message.downgrade(), open_issue,
    };
    view.render_saved();
    let first = view.saved.borrow().first().map(|d| d.id.clone());
    if let Some(id) = first { view.select(&id); }
    else { note(&body, "Choose or add a dashboard."); }

    let available = Rc::new(RefCell::new(Vec::<Dashboard>::new()));
    {
        let view = view.clone();
        let available = available.clone();
        let chooser = chooser.downgrade();
        add.connect_clicked(move |_| {
            let Some(chooser) = chooser.upgrade() else { return; };
            let selected = available.borrow().get(chooser.selected() as usize).cloned();
            if let Some(dashboard) = selected { view.add(dashboard); }
        });
    }
    {
        let view = view.clone();
        let entry = entry.downgrade();
        add_url.connect_clicked(move |button| {
            let Some(entry) = entry.upgrade() else { return; };
            let id = match dashboards::parse_dashboard_id(&entry.text(), &view.account.base_url) {
                Ok(id) => id,
                Err(error) => { view.message(&error.to_string()); return; }
            };
            button.set_sensitive(false);
            let button = button.downgrade();
            let account = view.account.clone();
            let view = view.clone();
            background(move || dashboards::by_id(&JiraClient::new(account)?, &id), move |result| {
                let Some(button) = button.upgrade() else { return; };
                button.set_sensitive(true);
                match result {
                    Ok(dashboard) => view.add(dashboard),
                    Err(error) => view.message(&format!("Could not add dashboard: {error}")),
                }
            });
        });
    }
    let account = view.account.clone();
    let chooser = chooser.downgrade();
    let add = add.downgrade();
    background(move || dashboards::list(&JiraClient::new(account)?), move |result| {
        let (Some(chooser), Some(add)) = (chooser.upgrade(), add.upgrade()) else { return; };
        match result {
            Ok(items) if !items.is_empty() => {
                let names: Vec<_> = items.iter().map(|d| d.name.as_str()).collect();
                chooser.set_model(Some(&gtk::StringList::new(&names)));
                chooser.set_selected(0);
                chooser.set_sensitive(true);
                add.set_sensitive(true);
                *available.borrow_mut() = items;
            }
            Ok(_) => view.message("No accessible dashboards. You can also try adding a URL."),
            Err(error) => view.message(&format!("Could not list dashboards: {error}")),
        }
    });
}
