use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::mpsc::{self, Sender},
    thread,
    time::Duration,
};

use adw::prelude::*;
use gtk::glib;

use crate::{
    jira::{AuthMode, Credentials, Issue, JiraClient, Project, ProjectStatus},
    preferences::{self, BoardSettings},
    secrets,
};

#[derive(Clone)]
struct Ui {
    window: adw::ApplicationWindow,
    toast: adw::ToastOverlay,
    header_title: adw::WindowTitle,
    spinner: gtk::Spinner,
    status: gtk::Label,
    projects_label: gtk::Label,
    connect_button: gtk::Button,
    disconnect_button: gtk::Button,
    refresh_button: gtk::Button,
    manage_projects_button: gtk::Button,
    manage_columns_button: gtk::Button,
    search: gtk::SearchEntry,
    center_title: gtk::Label,
    board_box: gtk::Box,
    detail_box: gtk::Box,
    mine_filter: gtk::ToggleButton,
    unassigned_filter: gtk::ToggleButton,
    all_filter: gtk::ToggleButton,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum IssueScope {
    #[default]
    Mine,
    Unassigned,
    All,
}

impl IssueScope {
    fn jql(self) -> Option<&'static str> {
        match self {
            Self::Mine => Some("assignee = currentUser()"),
            Self::Unassigned => Some("assignee IS EMPTY"),
            Self::All => None,
        }
    }
}

#[derive(Default)]
struct State {
    account: Option<Credentials>,
    projects: Vec<Project>,
    settings: BoardSettings,
    statuses: Vec<ProjectStatus>,
    issues: Vec<Issue>,
    issue_scope: IssueScope,
    generation: u64,
    board_request: u64,
    busy: bool,
}

struct Loaded {
    user: String,
    projects: Result<Vec<Project>, String>,
}

struct BoardData {
    statuses: Vec<ProjectStatus>,
    issues: Vec<Issue>,
}

enum Event {
    Stored(Result<Option<Credentials>, String>),
    Loaded(u64, Credentials, Result<Loaded, String>),
    BoardLoaded(u64, u64, Result<BoardData, String>),
    Disconnected(u64, Result<(), String>),
}

pub fn run() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("com.github.ziacik.DeskForJira")
        .build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &adw::Application) {
    let state = Rc::new(RefCell::new(State::default()));
    let (sender, receiver) = mpsc::channel();
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Desk for Jira")
        .default_width(1320)
        .default_height(790)
        .width_request(920)
        .height_request(520)
        .build();

    let header = adw::HeaderBar::new();
    let header_title = adw::WindowTitle::new("Desk for Jira", "My project board");
    header.set_title_widget(Some(&header_title));
    let connect_button = gtk::Button::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text("Connect or switch Jira account")
        .build();
    let disconnect_button = gtk::Button::builder()
        .icon_name("system-log-out-symbolic")
        .tooltip_text("Disconnect and erase saved credentials")
        .build();
    let refresh_button = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Refresh Jira data")
        .build();
    let spinner = gtk::Spinner::new();
    spinner.set_visible(false);
    header.pack_start(&connect_button);
    header.pack_end(&disconnect_button);
    header.pack_end(&refresh_button);
    header.pack_end(&spinner);

    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 12);
    sidebar.set_width_request(230);
    sidebar.set_margin_top(18);
    sidebar.set_margin_bottom(14);
    sidebar.set_margin_start(14);
    sidebar.set_margin_end(14);
    let sidebar_title = gtk::Label::new(Some("MY BOARD"));
    sidebar_title.add_css_class("heading");
    sidebar_title.set_xalign(0.0);
    sidebar.append(&sidebar_title);
    let projects_label = gtk::Label::new(Some("Choose projects to begin"));
    projects_label.set_wrap(true);
    projects_label.set_xalign(0.0);
    projects_label.add_css_class("dim-label");
    sidebar.append(&projects_label);
    let manage_projects_button = gtk::Button::with_label("Choose projects…");
    manage_projects_button.set_sensitive(false);
    sidebar.append(&manage_projects_button);
    let manage_columns_button = gtk::Button::with_label("Edit columns…");
    manage_columns_button.set_sensitive(false);
    sidebar.append(&manage_columns_button);
    let status = gtk::Label::new(Some("Connecting to keyring…"));
    status.add_css_class("dim-label");
    status.set_xalign(0.0);
    status.set_wrap(true);
    status.set_margin_top(16);
    sidebar.append(&status);

    let center = gtk::Box::new(gtk::Orientation::Vertical, 10);
    center.set_margin_top(16);
    center.set_margin_start(16);
    center.set_margin_end(12);
    center.set_margin_bottom(12);
    center.set_hexpand(true);
    let center_title = gtk::Label::new(Some("My project board"));
    center_title.add_css_class("title-2");
    center_title.set_xalign(0.0);
    center.append(&center_title);
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Filter by key, title or status"));
    center.append(&search);

    let mine_filter = gtk::ToggleButton::with_label("Mine");
    let unassigned_filter = gtk::ToggleButton::with_label("Unassigned");
    let all_filter = gtk::ToggleButton::with_label("All");
    unassigned_filter.set_group(Some(&mine_filter));
    all_filter.set_group(Some(&mine_filter));
    mine_filter.set_active(true);
    let filters = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    filters.add_css_class("linked");
    for button in [&mine_filter, &unassigned_filter, &all_filter] {
        filters.append(button);
    }
    center.append(&filters);

    let board_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    board_box.set_halign(gtk::Align::Start);
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Never);
    scroller.set_vexpand(true);
    scroller.set_child(Some(&board_box));
    center.append(&scroller);

    let detail_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
    detail_box.set_margin_top(20);
    detail_box.set_margin_start(16);
    detail_box.set_margin_end(16);
    detail_box.set_margin_bottom(20);
    detail_box.set_width_request(300);
    let detail_scroll = gtk::ScrolledWindow::new();
    detail_scroll.set_child(Some(&detail_box));
    detail_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);

    let inner_split = gtk::Paned::new(gtk::Orientation::Horizontal);
    inner_split.set_start_child(Some(&center));
    inner_split.set_end_child(Some(&detail_scroll));
    inner_split.set_position(560);
    inner_split.set_resize_start_child(true);
    inner_split.set_shrink_end_child(false);
    let split = gtk::Paned::new(gtk::Orientation::Horizontal);
    split.set_start_child(Some(&sidebar));
    split.set_end_child(Some(&inner_split));
    split.set_position(240);
    split.set_resize_start_child(false);
    split.set_shrink_start_child(false);
    let toast = adw::ToastOverlay::new();
    toast.set_child(Some(&split));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toast));
    window.set_content(Some(&toolbar));

    let ui = Ui {
        window, toast, header_title, spinner, status, projects_label,
        connect_button, disconnect_button, refresh_button,
        manage_projects_button, manage_columns_button, search, center_title,
        board_box, detail_box, mine_filter, unassigned_filter, all_filter,
    };
    show_detail_placeholder(&ui.detail_box);
    show_empty(&ui.board_box, "Connect to Jira", "Your unified project board will appear here.");
    wire_actions(&ui, &state, &sender);
    {
        let ui = ui.clone();
        let state = state.clone();
        let sender = sender.clone();
        glib::timeout_add_local(Duration::from_millis(90), move || {
            while let Ok(event) = receiver.try_recv() {
                handle_event(&ui, &state, &sender, event);
            }
            glib::ControlFlow::Continue
        });
    }
    thread::spawn(move || {
        let _ = sender.send(Event::Stored(secrets::load().map_err(|err| err.to_string())));
    });
    ui.window.present();
}

fn wire_actions(ui: &Ui, state: &Rc<RefCell<State>>, sender: &Sender<Event>) {
    for (button, scope) in [
        (ui.mine_filter.clone(), IssueScope::Mine),
        (ui.unassigned_filter.clone(), IssueScope::Unassigned),
        (ui.all_filter.clone(), IssueScope::All),
    ] {
        let ui = ui.clone();
        let state = state.clone();
        let sender = sender.clone();
        button.connect_toggled(move |button| {
            if !button.is_active() { return; }
            let changed = {
                let mut s = state.borrow_mut();
                if s.issue_scope == scope { false } else {
                    s.issue_scope = scope;
                    true
                }
            };
            if changed { start_board_load(&ui, &state, &sender); }
        });
    }
    {
        let ui = ui.clone();
        let state = state.clone();
        let sender = sender.clone();
        let button = ui.manage_projects_button.clone();
        button.connect_clicked(move |_| show_project_picker(&ui, &state, &sender));
    }
    {
        let ui = ui.clone();
        let state = state.clone();
        let button = ui.manage_columns_button.clone();
        button.connect_clicked(move |_| show_column_editor(&ui, &state));
    }
    {
        let ui = ui.clone();
        let state = state.clone();
        let sender = sender.clone();
        let button = ui.connect_button.clone();
        button.connect_clicked(move |_| show_login(&ui, &state, &sender));
    }
    {
        let ui = ui.clone();
        let state = state.clone();
        let sender = sender.clone();
        let button = ui.refresh_button.clone();
        button.connect_clicked(move |_| {
            let account = { state.borrow().account.clone() };
            if let Some(account) = account {
                start_refresh(&ui, &state, &sender, account, false);
            } else {
                show_login(&ui, &state, &sender);
            }
        });
    }
    {
        let ui = ui.clone();
        let state = state.clone();
        let button = ui.search.clone();
        button.connect_search_changed(move |_| render_board(&ui, &state.borrow()));
    }
    {
        let ui = ui.clone();
        let state = state.clone();
        let sender = sender.clone();
        let button = ui.disconnect_button.clone();
        button.connect_clicked(move |_| {
            let id = {
                let mut s = state.borrow_mut();
                s.generation += 1;
                s.board_request += 1;
                s.account = None;
                s.projects.clear();
                s.issues.clear();
                s.statuses.clear();
                s.settings = BoardSettings::default();
                s.issue_scope = IssueScope::Mine;
                s.generation
            };
            set_busy(&ui, &state, false);
            ui.header_title.set_subtitle("Not connected");
            ui.status.set_text("Disconnected");
            ui.projects_label.set_text("Choose projects to begin");
            ui.manage_projects_button.set_sensitive(false);
            ui.manage_columns_button.set_sensitive(false);
            ui.center_title.set_text("My project board");
            ui.mine_filter.set_active(true);
            show_empty(&ui.board_box, "Not connected", "Connect to Jira to view issues.");
            show_detail_placeholder(&ui.detail_box);
            let sender = sender.clone();
            thread::spawn(move || {
                let _ = sender.send(Event::Disconnected(id, secrets::delete().map_err(|e| e.to_string())));
            });
        });
    }
}

fn handle_event(ui: &Ui, state: &Rc<RefCell<State>>, sender: &Sender<Event>, event: Event) {
    match event {
        Event::Stored(Ok(Some(account))) => start_refresh(ui, state, sender, account, false),
        Event::Stored(Ok(None)) => {
            ui.status.set_text("No saved Jira account");
            show_login(ui, state, sender);
        }
        Event::Stored(Err(error)) => {
            show_error(ui, &error);
            ui.status.set_text("Unable to read keyring");
            show_login(ui, state, sender);
        }
        Event::Loaded(id, account, result) => {
            if state.borrow().generation != id { return; }
            set_busy(ui, state, false);
            match result {
                Ok(loaded) => {
                    let settings = preferences::load(&account.base_url).unwrap_or_else(|err| {
                        show_error(ui, &format!("Could not read board settings: {err}"));
                        BoardSettings::default()
                    });
                    let project_error = loaded.projects.as_ref().err().cloned();
                    {
                        let mut s = state.borrow_mut();
                        s.account = Some(account);
                        s.projects = loaded.projects.unwrap_or_default();
                        s.settings = settings;
                        s.statuses.clear();
                        s.issues.clear();
                        s.issue_scope = IssueScope::Mine;
                    }
                    ui.mine_filter.set_active(true);
                    ui.manage_projects_button.set_sensitive(true);
                    ui.manage_columns_button.set_sensitive(false);
                    ui.header_title.set_subtitle(&loaded.user);
                    update_project_label(ui, &state.borrow());
                    show_detail_placeholder(&ui.detail_box);
                    if let Some(error) = project_error {
                        show_error(ui, &format!("Could not list projects: {error}"));
                    }
                    start_board_load(ui, state, sender);
                }
                Err(error) => {
                    ui.status.set_text("Connection failed");
                    show_error(ui, &error);
                }
            }
        }
        Event::BoardLoaded(id, request, result) => {
            if state.borrow().generation != id || state.borrow().board_request != request { return; }
            set_busy(ui, state, false);
            match result {
                Ok(data) => {
                    let (base_url, settings, issue_count) = {
                        let mut s = state.borrow_mut();
                        s.issues = data.issues;
                        s.statuses = data.statuses;
                        let statuses = s.statuses.clone();
                        s.settings.reconcile(&statuses);
                        (
                            s.account.as_ref().map(|a| a.base_url.clone()).unwrap_or_default(),
                            s.settings.clone(),
                            s.issues.len(),
                        )
                    };
                    if let Err(error) = preferences::save(&base_url, &settings) {
                        show_error(ui, &format!("Could not save board settings: {error}"));
                    }
                    ui.manage_columns_button.set_sensitive(!state.borrow().statuses.is_empty());
                    ui.status.set_text(&format!("{issue_count} issues · {} projects", settings.projects.len()));
                    render_board(ui, &state.borrow());
                }
                Err(error) => {
                    show_empty(&ui.board_box, "Could not load project board", &error);
                    show_error(ui, &error);
                }
            }
        }
        Event::Disconnected(id, result) => {
            if state.borrow().generation != id { return; }
            if let Err(error) = result { show_error(ui, &error); }
        }
    }
}

fn start_refresh(ui: &Ui, state: &Rc<RefCell<State>>, sender: &Sender<Event>, account: Credentials, save: bool) {
    let id = {
        let mut s = state.borrow_mut();
        s.generation += 1;
        s.board_request += 1;
        s.generation
    };
    set_busy(ui, state, true);
    ui.status.set_text("Loading Jira…");
    let sender = sender.clone();
    thread::spawn(move || {
        let result = (|| -> anyhow::Result<Loaded> {
            let client = JiraClient::new(account.clone())?;
            let user = client.myself()?;
            let projects = client.projects().map_err(|e| e.to_string());
            if save { secrets::save(&account)?; }
            Ok(Loaded { user, projects })
        })().map_err(|err| err.to_string());
        let _ = sender.send(Event::Loaded(id, account, result));
    });
}

fn start_board_load(ui: &Ui, state: &Rc<RefCell<State>>, sender: &Sender<Event>) {
    let (id, request, account, projects, scope) = {
        let mut s = state.borrow_mut();
        s.board_request += 1;
        (s.generation, s.board_request, s.account.clone(), s.settings.projects.clone(), s.issue_scope)
    };
    update_project_label(ui, &state.borrow());
    if projects.is_empty() {
        set_busy(ui, state, false);
        ui.status.set_text("Select one or more projects");
        ui.manage_columns_button.set_sensitive(false);
        show_empty(&ui.board_box, "Choose projects", "Select ART, AFB or any other projects to build your board.");
        return;
    }
    let Some(account) = account else { return; };
    ui.status.set_text("Loading project issues and workflow statuses…");
    show_empty(&ui.board_box, "Loading board…", "Fetching real Jira workflow statuses");
    show_detail_placeholder(&ui.detail_box);
    set_busy(ui, state, true);
    let sender = sender.clone();
    thread::spawn(move || {
        let result = (|| -> anyhow::Result<BoardData> {
            let client = JiraClient::new(account)?;
            let statuses = client.project_statuses(&projects)?;
            let issues = client.project_issues(&projects, scope.jql())?;
            Ok(BoardData { statuses, issues })
        })().map_err(|err| err.to_string());
        let _ = sender.send(Event::BoardLoaded(id, request, result));
    });
}

fn update_project_label(ui: &Ui, state: &State) {
    if state.settings.projects.is_empty() {
        ui.projects_label.set_text("No projects selected");
        ui.center_title.set_text("My project board");
    } else {
        let keys = state.settings.projects.join(" · ");
        ui.projects_label.set_text(&keys);
        ui.center_title.set_text(&format!("Board · {keys}"));
    }
}

fn show_login(ui: &Ui, state: &Rc<RefCell<State>>, sender: &Sender<Event>) {
    let dialog = gtk::Window::builder()
        .title("Connect to Jira")
        .transient_for(&ui.window)
        .modal(true)
        .default_width(460)
        .resizable(false)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(22);
    content.set_margin_bottom(22);
    content.set_margin_start(22);
    content.set_margin_end(22);

    let mode = gtk::DropDown::from_strings(&["Jira Cloud", "Jira Server / Data Center"]);
    let site = gtk::Entry::new();
    site.set_placeholder_text(Some("https://yourcompany.atlassian.net"));
    let email = gtk::Entry::new();
    email.set_placeholder_text(Some("Your Atlassian account email"));
    let token = gtk::PasswordEntry::new();
    token.set_placeholder_text(Some("API token / personal access token"));
    token.set_show_peek_icon(true);

    if let Some(account) = state.borrow().account.as_ref() {
        mode.set_selected(if account.mode == AuthMode::Cloud { 0 } else { 1 });
        site.set_text(&account.base_url);
        email.set_text(&account.email);
        token.set_text(&account.token);
    }
    email.set_sensitive(mode.selected() == 0);
    {
        let email = email.clone();
        mode.connect_selected_notify(move |dropdown| email.set_sensitive(dropdown.selected() == 0));
    }

    content.append(&field("Jira type", &mode));
    content.append(&field("Jira base URL", &site));
    content.append(&field("Email (Cloud only)", &email));
    content.append(&field("Token", &token));

    let hint = gtk::Label::new(Some("Credentials are stored in the system keyring. HTTPS is required."));
    hint.set_wrap(true);
    hint.add_css_class("dim-label");
    content.append(&hint);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let connect = gtk::Button::with_label("Connect");
    connect.add_css_class("suggested-action");
    actions.append(&cancel);
    actions.append(&connect);
    content.append(&actions);
    dialog.set_child(Some(&content));

    {
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| dialog.close());
    }
    {
        let ui = ui.clone();
        let state = state.clone();
        let sender = sender.clone();
        let dialog = dialog.clone();
        connect.connect_clicked(move |_| {
            let mut account = Credentials {
                base_url: site.text().to_string(),
                mode: if mode.selected() == 0 { AuthMode::Cloud } else { AuthMode::Server },
                email: email.text().to_string(),
                token: token.text().to_string(),
            };
            match account.validate() {
                Ok(()) => {
                    dialog.close();
                    start_refresh(&ui, &state, &sender, account, true);
                }
                Err(error) => show_error(&ui, &error.to_string()),
            }
        });
    }
    dialog.present();
}

fn field<T: IsA<gtk::Widget>>(label: &str, widget: &T) -> gtk::Box {
    let group = gtk::Box::new(gtk::Orientation::Vertical, 5);
    let label = gtk::Label::new(Some(label));
    label.set_xalign(0.0);
    label.add_css_class("heading");
    group.append(&label);
    group.append(widget);
    group
}

fn show_project_picker(ui: &Ui, state: &Rc<RefCell<State>>, sender: &Sender<Event>) {
    let (projects, selected) = {
        let s = state.borrow();
        (s.projects.clone(), s.settings.projects.clone())
    };
    let dialog = gtk::Window::builder()
        .title("Choose projects")
        .transient_for(&ui.window)
        .modal(true)
        .default_width(480)
        .default_height(560)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    let intro = gtk::Label::new(Some("Select the projects to combine in your board."));
    intro.set_xalign(0.0);
    intro.set_wrap(true);
    content.append(&intro);
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search project key or name"));
    content.append(&search);
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");
    let mut choices = Vec::new();
    let mut searchable = Vec::new();
    for project in projects {
        let check = gtk::CheckButton::with_label(&format!("{} · {}", project.key, project.name));
        check.set_active(selected.contains(&project.key));
        check.set_margin_start(12);
        check.set_margin_end(12);
        check.set_margin_top(10);
        check.set_margin_bottom(10);
        let row = gtk::ListBoxRow::new();
        row.set_activatable(false);
        row.set_child(Some(&check));
        list.append(&row);
        searchable.push((row, format!("{} {}", project.key, project.name).to_lowercase()));
        choices.push((project.key, check));
    }
    search.connect_search_changed(move |search| {
        let text = search.text().to_lowercase();
        for (row, key) in &searchable { row.set_visible(key.contains(&text)); }
    });
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&list));
    content.append(&scroll);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let apply = gtk::Button::with_label("Apply");
    apply.add_css_class("suggested-action");
    actions.append(&cancel);
    actions.append(&apply);
    content.append(&actions);
    dialog.set_child(Some(&content));
    {
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| dialog.close());
    }
    {
        let dialog = dialog.clone();
        let ui = ui.clone();
        let state = state.clone();
        let sender = sender.clone();
        apply.connect_clicked(move |_| {
            let selected: Vec<String> = choices.iter()
                .filter(|(_, check)| check.is_active())
                .map(|(key, _)| key.clone()).collect();
            if selected != state.borrow().settings.projects {
                state.borrow_mut().settings.projects = selected;
                save_settings(&ui, &state);
                start_board_load(&ui, &state, &sender);
            }
            dialog.close();
        });
    }
    dialog.present();
    search.grab_focus();
}

// A series of clicks at one screen position must keep moving the SAME column,
// even though a different card slides underneath that position after each move.
#[derive(Clone)]
struct ColumnReorderSession {
    reference: preferences::StatusRef,
    card: gtk::Widget,
    left: bool,
    press: (f64, f64),
}

#[derive(Clone, Default)]
struct ColumnReorderContext {
    active: Rc<RefCell<Option<ColumnReorderSession>>>,
    last_press: Rc<Cell<Option<(f64, f64)>>>,
}

fn same_click_position(a: (f64, f64), b: (f64, f64)) -> bool {
    (a.0 - b.0).abs() <= 3.0 && (a.1 - b.1).abs() <= 3.0
}

fn move_editor_column(
    rows: &gtk::Box,
    ui: &Ui,
    state: &Rc<RefCell<State>>,
    card: &gtk::Widget,
    reference: &preferences::StatusRef,
    left: bool,
) -> bool {
    let neighbor = if left { card.prev_sibling() } else { card.next_sibling() };
    let Some(neighbor) = neighbor else { return false; };
    if !state.borrow_mut().settings.move_visible_column(reference, left) {
        return false;
    }
    if left {
        let before = neighbor.prev_sibling();
        rows.reorder_child_after(card, before.as_ref());
    } else {
        rows.reorder_child_after(card, Some(&neighbor));
    }
    update_editor_positions(rows, state);
    save_settings(ui, state);
    render_board(ui, &state.borrow());
    true
}

fn show_column_editor(ui: &Ui, state: &Rc<RefCell<State>>) {
    let dialog = gtk::Window::builder()
        .title("Board columns")
        .transient_for(&ui.window)
        .modal(true)
        .default_width(680)
        .default_height(720)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);

    let intro = gtk::Label::new(Some(
        "Each card below is one board column. Cards are ordered left to right on your board. \
         Add Jira statuses to a column to combine them; Separate gives a status its own column."
    ));
    intro.set_wrap(true);
    intro.set_xalign(0.0);
    content.append(&intro);

    let rows = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let reorder = ColumnReorderContext::default();

    // Capture mouse presses before any card button sees them. A repeated press
    // at the same dialog coordinate is routed to the ORIGINAL column, rather
    // than whichever different card is now underneath the stationary mouse.
    let clicks = gtk::GestureClick::new();
    clicks.set_button(1);
    clicks.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let reorder = reorder.clone();
        let rows = rows.clone();
        let ui = ui.clone();
        let state = state.clone();
        clicks.connect_pressed(move |gesture, _, x, y| {
            let repeat = reorder.active.borrow().as_ref()
                .filter(|active| active.card.parent().is_some()
                    && same_click_position(active.press, (x, y)))
                .cloned();
            if let Some(active) = repeat {
                // Prevent GTK from also clicking the unrelated button under
                // the pointer; that would swap columns back and forth.
                gesture.set_state(gtk::EventSequenceState::Claimed);
                move_editor_column(
                    &rows, &ui, &state, &active.card, &active.reference, active.left,
                );
            } else {
                reorder.active.borrow_mut().take();
                reorder.last_press.set(Some((x, y)));
            }
        });
    }
    dialog.add_controller(clicks);

    let pointer = gtk::EventControllerMotion::new();
    {
        let reorder = reorder.clone();
        pointer.connect_motion(move |_, x, y| {
            let active = reorder.active.borrow().as_ref()
                .is_some_and(|session| !same_click_position(session.press, (x, y)));
            if active {
                reorder.active.borrow_mut().take();
            }
        });
    }
    dialog.add_controller(pointer);
    rebuild_editor(&rows, ui, state, &dialog, &reorder);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_child(Some(&rows));
    content.append(&scroll);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let reset = gtk::Button::with_label("Reset columns");
    reset.set_tooltip_text(Some("Restore columns from Jira status names"));
    actions.append(&reset);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    actions.append(&spacer);
    let done = gtk::Button::with_label("Done");
    done.add_css_class("suggested-action");
    actions.append(&done);
    content.append(&actions);
    dialog.set_child(Some(&content));
    {
        let dialog = dialog.clone();
        done.connect_clicked(move |_| dialog.close());
    }
    {
        let ui = ui.clone();
        let state = state.clone();
        let rows = rows.clone();
        let editor = dialog.clone();
        let reorder = reorder.clone();
        reset.connect_clicked(move |_| confirm_column_reset(&ui, &state, &rows, &editor, &reorder));
    }
    dialog.present();
}

fn rebuild_editor(
    rows: &gtk::Box,
    ui: &Ui,
    state: &Rc<RefCell<State>>,
    editor: &gtk::Window,
    reorder: &ColumnReorderContext,
) {
    reorder.active.borrow_mut().take();
    clear_box(rows);
    let (columns, statuses, projects) = {
        let s = state.borrow();
        (s.settings.columns.clone(), s.statuses.clone(), s.settings.projects.clone())
    };
    let visible: Vec<usize> = columns.iter().enumerate()
        .filter(|(_, column)| column.statuses.iter().any(|s| projects.contains(&s.project_key)))
        .map(|(index, _)| index)
        .collect();

    for (position, &index) in visible.iter().enumerate() {
        let column = &columns[index];
        let matching: Vec<ProjectStatus> = column.statuses.iter()
            .filter(|reference| projects.contains(&reference.project_key))
            .filter_map(|reference| statuses.iter()
                .find(|status| status.reference() == *reference).cloned())
            .collect();
        if matching.is_empty() { continue; }

        let card = gtk::Box::new(gtk::Orientation::Vertical, 10);
        card.add_css_class("card");
        card.set_margin_bottom(4);
        card.set_margin_start(2);
        card.set_margin_end(2);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.set_margin_top(12);
        header.set_margin_start(12);
        header.set_margin_end(12);
        let heading = gtk::Label::new(Some(&format!(
            "{}. {}", position + 1, column_label(column, &columns, &projects)
        )));
        heading.set_xalign(0.0);
        heading.set_hexpand(true);
        heading.add_css_class("heading");
        heading.set_ellipsize(gtk::pango::EllipsizeMode::End);
        header.append(&heading);

        for (up, icon, tooltip) in [
            (true, "go-previous-symbolic", "Move column left"),
            (false, "go-next-symbolic", "Move column right"),
        ] {
            let destination = if up {
                position.checked_sub(1).and_then(|i| visible.get(i)).copied()
            } else {
                visible.get(position + 1).copied()
            };
            let button = gtk::Button::builder().icon_name(icon).build();
            button.set_tooltip_text(Some(tooltip));
            button.set_sensitive(destination.is_some());
            header.append(&button);
            let ui = ui.clone();
            let state = state.clone();
            let rows = rows.clone();
            let anchor = column.statuses[0].clone();
            let reorder = reorder.clone();
            button.connect_clicked(move |button| {
                let Some(card) = button.parent().and_then(|header| header.parent())
                else { return; };
                if move_editor_column(&rows, &ui, &state, &card, &anchor, up) {
                    if let Some(press) = reorder.last_press.get() {
                        *reorder.active.borrow_mut() = Some(ColumnReorderSession {
                            reference: anchor.clone(),
                            card,
                            left: up,
                            press,
                        });
                    }
                }
            });
        }
        card.append(&header);

        let name_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        name_row.set_margin_start(12);
        name_row.set_margin_end(12);
        let name = gtk::Entry::new();
        name.set_text(&column.title);
        name.set_placeholder_text(Some("Column name"));
        name.set_tooltip_text(Some("Rename this board column without changing Jira statuses"));
        name.set_hexpand(true);
        name_row.append(&name);
        let rename = gtk::Button::with_label("Rename");
        rename.set_sensitive(false);
        name_row.append(&rename);
        card.append(&name_row);
        {
            let rename = rename.clone();
            let original = column.title.clone();
            name.connect_changed(move |entry| {
                let proposed = entry.text();
                rename.set_sensitive(!proposed.trim().is_empty() && proposed.trim() != original);
            });
        }
        {
            let rename = rename.clone();
            name.connect_activate(move |_| rename.emit_clicked());
        }
        {
            let ui = ui.clone();
            let state = state.clone();
            let rows = rows.clone();
            let editor = editor.clone();
            let anchor = column.statuses[0].clone();
            let reorder = reorder.clone();
            rename.connect_clicked(move |_| {
                let renamed = {
                    let mut s = state.borrow_mut();
                    let index = s.settings.columns.iter()
                        .position(|column| column.statuses.contains(&anchor));
                    index.is_some_and(|index| s.settings.rename_column(index, &name.text()))
                };
                if renamed {
                    refresh_column_editor(&ui, &state, &rows, &editor, &reorder);
                }
            });
        }

        let subheading = gtk::Label::new(Some(&format!(
            "Jira statuses in this column ({})", matching.len()
        )));
        subheading.set_xalign(0.0);
        subheading.set_margin_start(12);
        subheading.set_margin_end(12);
        subheading.add_css_class("dim-label");
        card.append(&subheading);

        for status in matching {
            let status_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            status_row.set_margin_start(12);
            status_row.set_margin_end(12);
            let label = gtk::Label::new(Some(&format!("{} · {}", status.project_key, status.name)));
            label.set_xalign(0.0);
            label.set_hexpand(true);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            label.set_tooltip_text(Some(&format!("Jira status ID: {}", status.id)));
            status_row.append(&label);

            if column.statuses.len() > 1 {
                let separate = gtk::Button::with_label("Separate");
                separate.set_tooltip_text(Some("Move this Jira status into its own new column"));
                status_row.append(&separate);
                let ui = ui.clone();
                let state = state.clone();
                let rows = rows.clone();
                let editor = editor.clone();
                let reorder = reorder.clone();
                separate.connect_clicked(move |_| {
                    state.borrow_mut().settings.move_status(
                        &status.reference(), None, &status.name
                    );
                    refresh_column_editor(&ui, &state, &rows, &editor, &reorder);
                });
            }
            card.append(&status_row);
        }

        let add = gtk::Button::with_label("+ Add Jira status…");
        add.set_halign(gtk::Align::Start);
        add.set_margin_start(12);
        add.set_margin_bottom(12);
        add.set_tooltip_text(Some("Choose a status from another column to combine them here"));
        card.append(&add);
        {
            let ui = ui.clone();
            let state = state.clone();
            let rows = rows.clone();
            let editor = editor.clone();
            let anchor = column.statuses[0].clone();
            let reorder = reorder.clone();
            add.connect_clicked(move |_| {
                let target = {
                    let s = state.borrow();
                    s.settings.columns.iter()
                        .position(|column| column.statuses.contains(&anchor))
                };
                if let Some(target) = target {
                    show_add_status_picker(&ui, &state, &rows, &editor, target, &reorder);
                }
            });
        }

        rows.append(&card);
    }
    if visible.is_empty() {
        let empty = gtk::Label::new(Some("No Jira statuses found for the selected projects."));
        empty.set_wrap(true);
        empty.add_css_class("dim-label");
        rows.append(&empty);
    }
}

fn update_editor_positions(rows: &gtk::Box, state: &Rc<RefCell<State>>) {
    let s = state.borrow();
    let columns = &s.settings.columns;
    let projects = &s.settings.projects;
    let visible: Vec<_> = columns.iter()
        .filter(|column| column.statuses.iter()
            .any(|status| projects.contains(&status.project_key)))
        .collect();

    let mut child = rows.first_child();
    for (position, column) in visible.iter().enumerate() {
        let Some(card) = child else { break; };
        let Some(header) = card.first_child() else { break; };
        let Some(heading) = header.first_child().and_downcast::<gtk::Label>()
        else { break; };
        let Some(left) = heading.next_sibling().and_downcast::<gtk::Button>()
        else { break; };
        let Some(right) = left.next_sibling().and_downcast::<gtk::Button>()
        else { break; };

        heading.set_label(&format!(
            "{}. {}", position + 1, column_label(column, columns, projects)
        ));
        left.set_sensitive(position > 0);
        right.set_sensitive(position + 1 < visible.len());
        child = card.next_sibling();
    }
}

fn show_add_status_picker(
    ui: &Ui,
    state: &Rc<RefCell<State>>,
    rows: &gtk::Box,
    editor: &gtk::Window,
    target: usize,
    reorder: &ColumnReorderContext,
) {
    let (columns, mut candidates, projects) = {
        let s = state.borrow();
        let candidates = s.statuses.iter()
            .filter(|status| !s.settings.columns[target].statuses.contains(&status.reference()))
            .cloned()
            .collect::<Vec<_>>();
        (s.settings.columns.clone(), candidates, s.settings.projects.clone())
    };
    candidates.sort_by(|a, b| a.project_key.cmp(&b.project_key)
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));

    let picker = gtk::Window::builder()
        .title("Add Jira status to column")
        .transient_for(editor)
        .modal(true)
        .default_width(540)
        .default_height(510)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    let destination = column_label(&columns[target], &columns, &projects);
    let intro = gtk::Label::new(Some(&format!(
        "Move a Jira status into “{destination}”. It will be removed from its old column."
    )));
    intro.set_wrap(true);
    intro.set_xalign(0.0);
    content.append(&intro);
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Find project or status"));
    content.append(&search);

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");
    let mut searchable = Vec::new();
    for status in candidates {
        let source = columns.iter()
            .find(|column| column.statuses.contains(&status.reference()));
        let old_name = source.map(|column| column_label(column, &columns, &projects))
            .unwrap_or_else(|| "Unmapped".into());
        let row = gtk::ListBoxRow::new();
        row.set_activatable(false);
        let contents = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        contents.set_margin_top(8);
        contents.set_margin_bottom(8);
        contents.set_margin_start(10);
        contents.set_margin_end(10);
        let labels = gtk::Box::new(gtk::Orientation::Vertical, 3);
        labels.set_hexpand(true);
        let name = gtk::Label::new(Some(&format!("{} · {}", status.project_key, status.name)));
        name.set_xalign(0.0);
        name.add_css_class("heading");
        labels.append(&name);
        let current = gtk::Label::new(Some(&format!("Currently in: {old_name}")));
        current.set_xalign(0.0);
        current.add_css_class("dim-label");
        current.set_ellipsize(gtk::pango::EllipsizeMode::End);
        labels.append(&current);
        contents.append(&labels);
        let add = gtk::Button::with_label("Add");
        contents.append(&add);
        row.set_child(Some(&contents));
        list.append(&row);
        searchable.push((row, format!("{} {} {old_name}", status.project_key, status.name).to_lowercase()));

        let ui = ui.clone();
        let state = state.clone();
        let rows = rows.clone();
        let editor = editor.clone();
        let picker = picker.clone();
        let reorder = reorder.clone();
        add.connect_clicked(move |_| {
            state.borrow_mut().settings.move_status(
                &status.reference(), Some(target), &status.name
            );
            refresh_column_editor(&ui, &state, &rows, &editor, &reorder);
            picker.close();
        });
    }
    {
        search.connect_search_changed(move |entry| {
            let query = entry.text().to_lowercase();
            for (row, text) in &searchable {
                row.set_visible(text.contains(&query));
            }
        });
    }

    let scroller = gtk::ScrolledWindow::new();
    scroller.set_vexpand(true);
    scroller.set_child(Some(&list));
    content.append(&scroller);
    if list.first_child().is_none() {
        let empty = gtk::Label::new(Some("All available Jira statuses are already in this column."));
        empty.set_wrap(true);
        content.append(&empty);
    }
    let cancel = gtk::Button::with_label("Close");
    cancel.set_halign(gtk::Align::End);
    content.append(&cancel);
    picker.set_child(Some(&content));
    {
        let picker = picker.clone();
        cancel.connect_clicked(move |_| picker.close());
    }
    picker.present();
    search.grab_focus();
}

fn confirm_column_reset(
    ui: &Ui,
    state: &Rc<RefCell<State>>,
    rows: &gtk::Box,
    editor: &gtk::Window,
    reorder: &ColumnReorderContext,
) {
    let dialog = gtk::Window::builder()
        .title("Reset board columns?")
        .transient_for(editor)
        .modal(true)
        .resizable(false)
        .default_width(430)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
    content.set_margin_top(20);
    content.set_margin_bottom(20);
    content.set_margin_start(20);
    content.set_margin_end(20);
    let message = gtk::Label::new(Some(
        "Reset column names, order and status mappings? Jira statuses with matching \
         names will be grouped automatically. Your selected projects will be kept."
    ));
    message.set_wrap(true);
    message.set_xalign(0.0);
    content.append(&message);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let reset = gtk::Button::with_label("Reset");
    reset.add_css_class("destructive-action");
    actions.append(&cancel);
    actions.append(&reset);
    content.append(&actions);
    dialog.set_child(Some(&content));
    {
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| dialog.close());
    }
    {
        let ui = ui.clone();
        let state = state.clone();
        let rows = rows.clone();
        let editor = editor.clone();
        let dialog = dialog.clone();
        let reorder = reorder.clone();
        reset.connect_clicked(move |_| {
            let statuses = state.borrow().statuses.clone();
            state.borrow_mut().settings.reset_columns(&statuses);
            refresh_column_editor(&ui, &state, &rows, &editor, &reorder);
            dialog.close();
        });
    }
    dialog.present();
}

fn refresh_column_editor(
    ui: &Ui,
    state: &Rc<RefCell<State>>,
    rows: &gtk::Box,
    editor: &gtk::Window,
    reorder: &ColumnReorderContext,
) {
    save_settings(ui, state);
    render_board(ui, &state.borrow());
    rebuild_editor(rows, ui, state, editor, reorder);
}

fn save_settings(ui: &Ui, state: &Rc<RefCell<State>>) {
    let (url, settings) = {
        let s = state.borrow();
        (s.account.as_ref().map(|a| a.base_url.clone()), s.settings.clone())
    };
    if let Some(url) = url
        && let Err(error) = preferences::save(&url, &settings)
    {
        show_error(ui, &format!("Could not save board settings: {error}"));
    }
}

fn column_label(
    column: &preferences::Column,
    all_columns: &[preferences::Column],
    projects: &[String],
) -> String {
    // The common case (ART: Open + AFB: Open) is just "Open".
    // Disambiguate only if the user has deliberately split equal-named columns.
    let duplicate = all_columns.iter().filter(|candidate| {
        candidate.title.eq_ignore_ascii_case(&column.title)
            && candidate.statuses.iter().any(|s| projects.contains(&s.project_key))
    }).count() > 1;
    if !duplicate {
        return column.title.clone();
    }
    let mut project_keys: Vec<&str> = column.statuses.iter()
        .filter(|status| projects.contains(&status.project_key))
        .map(|status| status.project_key.as_str())
        .collect();
    project_keys.sort_unstable();
    project_keys.dedup();
    format!("{} · {}", column.title, project_keys.join(", "))
}

fn render_board(ui: &Ui, state: &State) {
    clear_box(&ui.board_box);
    if state.settings.projects.is_empty() {
        show_empty(&ui.board_box, "Choose projects", "Use Choose projects… to set up your board.");
        return;
    }
    let text = ui.search.text().to_lowercase();
    let mut groups: Vec<(String, Vec<&Issue>, Vec<preferences::StatusRef>)> = state.settings.columns.iter()
        .filter(|c| c.statuses.iter().any(|s| state.settings.projects.contains(&s.project_key)))
        .map(|c| (column_label(c, &state.settings.columns, &state.settings.projects), Vec::new(), c.statuses.clone())).collect();
    for issue in &state.issues {
        if !text.is_empty()
            && !issue.key.to_lowercase().contains(&text)
            && !issue.summary.to_lowercase().contains(&text)
            && !issue.status.to_lowercase().contains(&text)
        {
            continue;
        }
        let reference = preferences::StatusRef {
            project_key: issue.project_key.clone(),
            status_id: issue.status_id.clone(),
        };
        if let Some((_, items, _)) = groups.iter_mut().find(|(_, _, refs)| refs.contains(&reference)) {
            items.push(issue);
        } else {
            let index = groups.iter().position(|(title, _, _)| title == "Other");
            if let Some(index) = index {
                groups[index].1.push(issue);
            } else {
                groups.push(("Other".into(), vec![issue], Vec::new()));
            }
        }
    }
    if groups.is_empty() {
        show_empty(&ui.board_box, "No workflow statuses", "There are no statuses to display.");
        return;
    }
    let url = state.account.as_ref().map(|a| a.base_url.as_str()).unwrap_or("");
    for (name, issues, _) in groups {
        let column = gtk::Box::new(gtk::Orientation::Vertical, 8);
        column.set_width_request(280);
        let title = gtk::Label::new(Some(&format!("{name}  ({})", issues.len())));
        title.add_css_class("heading");
        title.set_xalign(0.0);
        column.append(&title);
        let item_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        for issue in issues {
            item_box.append(&issue_button(issue, &ui.detail_box, url));
        }
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_child(Some(&item_box));
        scroll.set_vexpand(true);
        column.append(&scroll);
        ui.board_box.append(&column);
    }
}

fn issue_button(issue: &Issue, detail: &gtk::Box, base_url: &str) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("card");
    let content = gtk::Box::new(gtk::Orientation::Vertical, 5);
    content.set_margin_top(10);
    content.set_margin_bottom(10);
    content.set_margin_start(10);
    content.set_margin_end(10);

    let key = gtk::Label::new(Some(&format!("{} · {}", issue.key, issue.status)));
    key.add_css_class("dim-label");
    key.set_xalign(0.0);
    content.append(&key);
    let summary = gtk::Label::new(Some(&issue.summary));
    summary.set_xalign(0.0);
    summary.set_wrap(true);
    content.append(&summary);
    button.set_child(Some(&content));

    let issue = issue.clone();
    let detail = detail.clone();
    let base_url = base_url.to_owned();
    button.connect_clicked(move |_| show_detail(&detail, &issue, &base_url));
    button
}

fn show_detail_placeholder(container: &gtk::Box) {
    show_empty(container, "Select an issue", "Choose a ticket to read its details.");
}

fn show_detail(container: &gtk::Box, issue: &Issue, base_url: &str) {
    clear_box(container);
    let key = gtk::Label::new(Some(&issue.key));
    key.set_xalign(0.0);
    key.add_css_class("dim-label");
    container.append(&key);

    let summary = gtk::Label::new(Some(&issue.summary));
    summary.add_css_class("title-2");
    summary.set_wrap(true);
    summary.set_selectable(true);
    summary.set_xalign(0.0);
    container.append(&summary);

    for (label, value) in [
        ("Status", issue.status.as_str()),
        ("Type", issue.kind.as_str()),
        ("Priority", issue.priority.as_str()),
        ("Assignee", issue.assignee.as_str()),
        ("Updated", issue.updated.as_str()),
    ] {
        if !value.is_empty() {
            let row = gtk::Label::new(Some(&format!("{label}: {value}")));
            row.set_wrap(true);
            row.set_xalign(0.0);
            row.set_selectable(true);
            container.append(&row);
        }
    }

    let section = gtk::Label::new(Some("Description"));
    section.add_css_class("heading");
    section.set_xalign(0.0);
    section.set_margin_top(14);
    container.append(&section);

    let description = gtk::Label::new(Some(if issue.description.is_empty() {
        "No description"
    } else {
        &issue.description
    }));
    description.set_wrap(true);
    description.set_selectable(true);
    description.set_xalign(0.0);
    description.set_valign(gtk::Align::Start);
    container.append(&description);

    if !base_url.is_empty() {
        let open = gtk::Button::with_label("Open in Jira ↗");
        open.set_halign(gtk::Align::Start);
        let url = format!("{base_url}/browse/{}", issue.key);
        open.connect_clicked(move |_| {
            let _ = gtk::gio::AppInfo::launch_default_for_uri(
                &url, None::<&gtk::gio::AppLaunchContext>
            );
        });
        container.append(&open);
    }
}

fn set_busy(ui: &Ui, state: &Rc<RefCell<State>>, busy: bool) {
    state.borrow_mut().busy = busy;
    ui.refresh_button.set_sensitive(!busy);
    ui.disconnect_button.set_sensitive(!busy);
    if busy {
        ui.spinner.set_visible(true);
        ui.spinner.start();
    } else {
        ui.spinner.stop();
        ui.spinner.set_visible(false);
    }
}

fn show_empty(container: &gtk::Box, title: &str, description: &str) {
    clear_box(container);
    let message = adw::StatusPage::builder()
        .icon_name("view-list-symbolic")
        .title(title)
        .description(description)
        .build();
    message.set_vexpand(true);
    container.append(&message);
}

fn clear_box(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn show_error(ui: &Ui, error: &str) {
    ui.toast.add_toast(adw::Toast::new(error));
}

#[cfg(test)]
mod scope_tests {
    use super::*;

    #[test]
    fn stationary_clicks_select_the_same_move_session() {
        assert!(same_click_position((280.0, 510.0), (280.0, 510.0)));
        assert!(same_click_position((280.0, 510.0), (281.0, 508.0)));
        assert!(!same_click_position((280.0, 510.0), (280.0, 485.0)));
    }

    #[test]
    fn board_scope_uses_server_side_jql() {
        assert_eq!(IssueScope::Mine.jql(), Some("assignee = currentUser()"));
        assert_eq!(IssueScope::Unassigned.jql(), Some("assignee IS EMPTY"));
        assert_eq!(IssueScope::All.jql(), None);
    }
}
