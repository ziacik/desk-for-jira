use std::{cell::{Cell, RefCell}, rc::Rc, sync::mpsc, thread, time::Duration};
use adw::prelude::*;
use gtk::glib;
use crate::{dashboards::{self, Dashboard, Gadget, Release}, jira::{Credentials, Issue, JiraClient}, preferences};

const PAGE_PREFIX: &str = "jira-dashboard-";
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


fn page_name(id: &str) -> String { format!("{PAGE_PREFIX}{id}") }

fn existing(tabs: &adw::TabView, id: &str) -> Option<adw::TabPage> {
    let name = page_name(id);
    (0..tabs.n_pages()).map(|i| tabs.nth_page(i))
        .find(|page| page.child().widget_name() == name.as_str())
}

pub fn close_one(tabs: &adw::TabView, id: &str) {
    if let Some(page) = existing(tabs, id) { tabs.close_page(&page); }
}

pub fn close(tabs: &adw::TabView) {
    let pages: Vec<_> = (0..tabs.n_pages()).map(|i| tabs.nth_page(i))
        .filter(|page| page.child().widget_name().starts_with(PAGE_PREFIX)).collect();
    for page in pages { tabs.close_page(&page); }
}

fn save_project(site: &str, id: &str, project: &str) -> anyhow::Result<Dashboard> {
    let mut saved = preferences::load_dashboards(site)?;
    let dashboard = saved.iter_mut().find(|d| d.id == id)
        .ok_or_else(|| anyhow::anyhow!("Dashboard is no longer saved"))?;
    dashboard.project = project.to_owned();
    let result = dashboard.clone();
    preferences::save_dashboards(site, &saved)?;
    Ok(result)
}

#[derive(Clone)]
struct View {
    account: Credentials,
    projects: Vec<String>,
    dashboard: Rc<RefCell<Dashboard>>,
    body: glib::WeakRef<gtk::Box>,
    open_issue: OpenIssue,
    generation: Rc<Cell<u64>>,
}

impl View {
    fn render(&self) {
        let Some(body) = self.body.upgrade() else { return; };
        self.generation.set(self.generation.get().wrapping_add(1));
        let generation = self.generation.get();
        let dashboard = self.dashboard.borrow().clone();
        clear(&body);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let title = heading(&dashboard.name);
        title.add_css_class("title-1");
        title.set_hexpand(true);
        header.append(&title);
        let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh.add_css_class("flat");
        refresh.set_tooltip_text(Some("Refresh dashboard"));
        let settings = gtk::MenuButton::builder().icon_name("emblem-system-symbolic")
            .tooltip_text("Configure local widgets").build();
        settings.add_css_class("flat");
        let url = format!("{}/jira/dashboards/{}", self.account.base_url, dashboard.id);
        let browser = gtk::LinkButton::with_label(&url, "Open in Jira");
        header.append(&refresh);
        header.append(&settings);
        header.append(&browser);
        body.append(&header);
        let view = self.clone();
        refresh.connect_clicked(move |_| view.render());

        // Release dates are a local optional widget, not part of every Jira
        // dashboard. An empty project means the widget is disabled.
        let popover = gtk::Popover::new();
        let menu = gtk::Box::new(gtk::Orientation::Vertical, 8);
        menu.set_margin_top(12);
        menu.set_margin_bottom(12);
        menu.set_margin_start(12);
        menu.set_margin_end(12);
        menu.append(&heading("Local widgets"));
        menu.append(&label("Upcoming releases"));
        let mut project_keys = self.projects.clone();
        if !dashboard.project.is_empty() && !project_keys.contains(&dashboard.project) {
            project_keys.push(dashboard.project.clone());
        }
        let mut names = vec!["Disabled"];
        names.extend(project_keys.iter().map(String::as_str));
        let project = gtk::DropDown::from_strings(&names);
        let selected = project_keys.iter().position(|p| p == &dashboard.project)
            .map_or(0, |i| i as u32 + 1);
        project.set_selected(selected);
        menu.append(&project);
        let apply = gtk::Button::with_label("Apply");
        menu.append(&apply);
        let error = label("");
        menu.append(&error);
        popover.set_child(Some(&menu));
        settings.set_popover(Some(&popover));
        {
            let view = self.clone();
            let project = project.downgrade();
            let error = error.downgrade();
            let popover = popover.downgrade();
            apply.connect_clicked(move |_| {
                let Some(project) = project.upgrade() else { return; };
                let key = project.selected().checked_sub(1)
                    .and_then(|i| project_keys.get(i as usize)).map(String::as_str).unwrap_or("");
                let id = view.dashboard.borrow().id.clone();
                match save_project(&view.account.base_url, &id, key) {
                    Ok(dashboard) => {
                        *view.dashboard.borrow_mut() = dashboard;
                        if let Some(popover) = popover.upgrade() { popover.popdown(); }
                        view.render();
                    }
                    Err(err) => if let Some(error) = error.upgrade() { error.set_text(&err.to_string()); },
                }
            });
        }

        if !dashboard.project.is_empty() {
            let group = widget_card();
            group.append(&heading("Upcoming releases"));
            note(&group, &format!("{} · Local widget", dashboard.project));
            let release_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
            group.append(&release_box);
            body.append(&group);
            load_releases(&release_box, &self.account, &dashboard.project, &self.open_issue);
        }

        let gadgets = gtk::Box::new(gtk::Orientation::Vertical, 12);
        body.append(&gadgets);
        note(&gadgets, "Loading Jira gadgets…");
        let account = self.account.clone();
        let id = dashboard.id;
        let view = self.clone();
        let gadgets = gadgets.downgrade();
        background(move || dashboards::gadgets(&JiraClient::new(account)?, &id), move |result| {
            if view.generation.get() != generation { return; }
            let Some(container) = gadgets.upgrade() else { return; };
            clear(&container);
            match result {
                Ok(items) if items.is_empty() => note(&container, "No dashboard gadgets returned by Jira."),
                Ok(items) => {
                    let mut unsupported = Vec::new();
                    let mut rendered = false;
                    for Gadget { title, data } in items {
                        match data {
                            Ok(items) => {
                                let group = widget_card();
                                group.append(&heading(&title));
                                issues(&group, &items, &view.open_issue);
                                container.append(&group);
                                rendered = true;
                            }
                            Err(error) => unsupported.push((title, error)),
                        }
                    }
                    if !rendered && !unsupported.is_empty() {
                        note(&container, "No gadgets on this dashboard can currently be displayed in Desk for Jira. Open in Jira to view the original dashboard.");
                    }
                    if !unsupported.is_empty() {
                        let expander = gtk::Expander::new(Some(&format!("Unavailable gadgets ({})", unsupported.len())));
                        let details = gtk::Box::new(gtk::Orientation::Vertical, 8);
                        details.set_margin_top(8);
                        for (title, error) in unsupported {
                            let line = label(&title);
                            line.add_css_class("heading");
                            details.append(&line);
                            note(&details, &error);
                        }
                        expander.set_child(Some(&details));
                        container.append(&expander);
                    }
                }
                Err(error) => note(&container, &format!("Could not load gadgets: {error}")),
            }
        });
    }
}

fn widget_card() -> gtk::Box {
    let group = gtk::Box::new(gtk::Orientation::Vertical, 8);
    group.add_css_class("dashboard-widget");
    group
}

pub fn show(tabs: &adw::TabView, account: Credentials, mut projects: Vec<String>, dashboard: Dashboard, open_issue: OpenIssue) {
    if let Some(page) = existing(tabs, &dashboard.id) {
        tabs.set_selected_page(&page);
        return;
    }
    projects.sort();
    projects.dedup();
    let body = gtk::Box::new(gtk::Orientation::Vertical, 14);
    body.set_margin_top(20);
    body.set_margin_start(24);
    body.set_margin_end(24);
    body.set_margin_bottom(20);
    body.set_hexpand(true);
    let scroll = gtk::ScrolledWindow::builder().hexpand(true).vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never).child(&body).build();
    scroll.set_widget_name(&page_name(&dashboard.id));
    let page = tabs.append(&scroll);
    page.set_title(&dashboard.name);
    tabs.set_selected_page(&page);
    let view = View {
        account, projects, dashboard: Rc::new(RefCell::new(dashboard)),
        body: body.downgrade(), open_issue, generation: Rc::new(Cell::new(0)),
    };
    view.render();
}

pub fn add_dialog(parent: &impl IsA<gtk::Window>, account: Credentials, saved: Vec<Dashboard>, add_dashboard: Rc<dyn Fn(Dashboard) -> bool>) {
    let window = gtk::Window::builder().title("Add dashboard").transient_for(parent)
        .modal(true).default_width(440).resizable(false).build();
    crate::dialog::close_on_escape(&window);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);
    content.append(&label("Choose a Jira dashboard"));
    let chooser = gtk::DropDown::from_strings(&["Loading dashboards…"]);
    chooser.set_sensitive(false);
    content.append(&chooser);
    let add = gtk::Button::with_label("Add dashboard");
    add.set_sensitive(false);
    content.append(&add);
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&label("Or paste a dashboard URL or ID"));
    let entry = gtk::Entry::builder().placeholder_text("Dashboard URL or ID").build();
    content.append(&entry);
    let add_url = gtk::Button::with_label("Add from URL");
    content.append(&add_url);
    let message = label("");
    content.append(&message);
    window.set_child(Some(&content));

    let available = Rc::new(RefCell::new(Vec::<Dashboard>::new()));
    {
        let available = available.clone();
        let chooser = chooser.downgrade();
        let window = window.downgrade();
        let callback = add_dashboard.clone();
        add.connect_clicked(move |_| {
            let Some(chooser) = chooser.upgrade() else { return; };
            let selected = available.borrow().get(chooser.selected() as usize).cloned();
            if let Some(dashboard) = selected && callback(dashboard)
                && let Some(window) = window.upgrade() { window.close(); }
        });
    }
    {
        let entry = entry.downgrade();
        let message = message.downgrade();
        let window = window.downgrade();
        let account = account.clone();
        let callback = add_dashboard;
        add_url.connect_clicked(move |button| {
            let Some(entry) = entry.upgrade() else { return; };
            let id = match dashboards::parse_dashboard_id(&entry.text(), &account.base_url) {
                Ok(id) => id,
                Err(error) => {
                    if let Some(message) = message.upgrade() { message.set_text(&error.to_string()); }
                    return;
                }
            };
            button.set_sensitive(false);
            let button = button.downgrade();
            let account = account.clone();
            let callback = callback.clone();
            let message = message.clone();
            let window = window.clone();
            background(move || dashboards::by_id(&JiraClient::new(account)?, &id), move |result| {
                let Some(window) = window.upgrade() else { return; };
                if !window.is_visible() { return; }
                if let Some(button) = button.upgrade() { button.set_sensitive(true); }
                match result {
                    Ok(dashboard) => if callback(dashboard) { window.close(); },
                    Err(error) => if let Some(message) = message.upgrade() { message.set_text(&error); },
                }
            });
        });
    }
    let chooser = chooser.downgrade();
    let add = add.downgrade();
    let message = message.downgrade();
    background(move || dashboards::list(&JiraClient::new(account)?), move |result| {
        let (Some(chooser), Some(add), Some(message)) = (chooser.upgrade(), add.upgrade(), message.upgrade()) else { return; };
        match result {
            Ok(items) => {
                let items: Vec<_> = items.into_iter().filter(|d| !saved.iter().any(|s| s.id == d.id)).collect();
                if items.is_empty() {
                    chooser.set_model(Some(&gtk::StringList::new(&["No more dashboards"])));
                    return;
                }
                let names: Vec<_> = items.iter().map(|d| d.name.as_str()).collect();
                chooser.set_model(Some(&gtk::StringList::new(&names)));
                chooser.set_selected(0);
                chooser.set_sensitive(true);
                add.set_sensitive(true);
                *available.borrow_mut() = items;
            }
            Err(error) => message.set_text(&format!("Could not list dashboards: {error}")),
        }
    });
    window.present();
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

