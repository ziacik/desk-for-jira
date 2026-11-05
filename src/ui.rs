use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    rc::Rc,
    sync::mpsc::{self, Sender},
    thread,
    time::Duration,
};

use adw::prelude::*;
use gtk::glib;

use crate::{
    issue_detail,
    jira::{AuthMode, Credentials, Issue, JiraClient, Project, ProjectStatus},
    preferences::{self, BoardSettings},
    secrets,
};

const COLUMN_WIDTH: i32 = 302;
const DETAIL_WIDTH: i32 = 470;
const COLLAPSED_COLUMN_WIDTH: i32 = 116;

#[derive(Clone)]
struct Ui {
    window: adw::ApplicationWindow,
    toast: adw::ToastOverlay,
    header_title: adw::WindowTitle,
    spinner: gtk::Spinner,
    status: gtk::Label,
    projects_label: gtk::Label,
    sidebar_projects: gtk::Box,
    project_filter: Rc<RefCell<Option<String>>>,
    connect_button: gtk::Button,
    disconnect_button: gtk::Button,
    refresh_button: gtk::Button,
    manage_projects_button: gtk::Button,
    manage_columns_button: gtk::Button,
    search: gtk::SearchEntry,
    center_title: gtk::Label,
    board_box: gtk::Box,
    detail_box: gtk::Box,
    detail_panel: gtk::Box,
    detail_heading: gtk::Label,
    saved_detail_width: Rc<Cell<i32>>,
    inner_split: gtk::Paned,
    image_slots: Rc<RefCell<issue_detail::ImageSlots>>,
    events: Sender<Event>,
    selected_issue: Rc<RefCell<Option<String>>>,
    selected_button: Rc<RefCell<Option<gtk::Button>>>,
    collapsed_columns: Rc<RefCell<HashSet<String>>>,
    mine_filter: gtk::ToggleButton,
    unassigned_filter: gtk::ToggleButton,
    all_filter: gtk::ToggleButton,
    sprint_filter: gtk::CheckButton,
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
    active_sprint_only: bool,
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
    ImageLoaded(u64, String, String, Result<Vec<u8>, String>),
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
        .default_width(1460)
        .default_height(860)
        .width_request(960)
        .height_request(580)
        .build();

    let header = adw::HeaderBar::new();
    let header_title = adw::WindowTitle::new("Desk for Jira", "Your Jira workspace");
    header.set_title_widget(Some(&header_title));
    let sidebar_toggle = gtk::ToggleButton::builder()
        .icon_name("sidebar-show-symbolic")
        .tooltip_text("Show or hide workspace sidebar")
        .build();
    sidebar_toggle.add_css_class("flat");
    sidebar_toggle.set_active(true);
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
    header.pack_start(&sidebar_toggle);
    header.pack_start(&connect_button);
    header.pack_end(&disconnect_button);
    header.pack_end(&refresh_button);
    header.pack_end(&spinner);


    let center = gtk::Box::new(gtk::Orientation::Vertical, 10);
    center.set_margin_top(16);
    center.set_margin_start(16);
    center.set_margin_end(16);
    center.set_margin_bottom(12);
    center.set_hexpand(true);
    center.set_width_request(300);

    let heading = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    heading.add_css_class("board-heading");
    let board_mark = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    board_mark.add_css_class("board-mark");
    board_mark.append(&gtk::Image::from_icon_name("view-grid-symbolic"));
    heading.append(&board_mark);
    let heading_text = gtk::Box::new(gtk::Orientation::Vertical, 3);
    heading_text.set_hexpand(true);
    let center_title = gtk::Label::new(Some("My board"));
    center_title.add_css_class("title-2");
    center_title.add_css_class("board-title");
    center_title.set_xalign(0.0);
    heading_text.append(&center_title);
    let projects_label = gtk::Label::new(Some("No projects selected"));
    projects_label.add_css_class("board-subtitle");
    projects_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    projects_label.set_xalign(0.0);
    heading_text.append(&projects_label);
    heading.append(&heading_text);
    center.append(&heading);

    let manage_projects_button = gtk::Button::builder()
        .icon_name("folder-open-symbolic")
        .tooltip_text("Choose projects for this board")
        .build();
    manage_projects_button.add_css_class("flat");
    manage_projects_button.add_css_class("sidebar-projects-action");
    manage_projects_button.set_sensitive(false);
    let manage_columns_button = gtk::Button::with_label("Edit columns");
    manage_columns_button.add_css_class("sidebar-action");
    manage_columns_button.set_sensitive(false);
    manage_columns_button.set_tooltip_text(Some("Edit status columns and their order"));

    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search issues… (/)"));
    search.set_width_request(190);
    search.set_hexpand(false);
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
    let sprint_filter = gtk::CheckButton::with_label("Active sprint only");
    sprint_filter.set_tooltip_text(Some("Show only issues in an active sprint"));
    // Filters and search share a single, compact toolbar.
    let filters_row = gtk::Box::new(gtk::Orientation::Horizontal, 9);
    filters_row.add_css_class("board-toolbar");
    filters.add_css_class("scope-filters");
    filters_row.append(&filters);
    filters_row.append(&sprint_filter);
    let status = gtk::Label::new(Some("Connecting to keyring…"));
    status.add_css_class("dim-label");
    status.set_hexpand(true);
    status.set_halign(gtk::Align::End);
    status.set_ellipsize(gtk::pango::EllipsizeMode::End);
    filters_row.append(&status);
    filters_row.append(&search);
    center.append(&filters_row);

    let board_box = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    // Columns retain a useful width even on ultra-wide monitors.
    // The horizontal scroller handles additional columns instead of stretching cards.
    board_box.set_hexpand(false);
    board_box.set_halign(gtk::Align::Start);
    let scroller = gtk::ScrolledWindow::new();
    scroller.add_css_class("board-scroll");
    scroller.set_min_content_width(320);
    scroller.set_propagate_natural_width(false);
    scroller.set_overlay_scrolling(false);
    scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Never);
    scroller.set_hexpand(true);
    scroller.set_vexpand(true);
    scroller.set_child(Some(&board_box));
    center.append(&scroller);

    let detail_panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
    detail_panel.set_width_request(390);
    detail_panel.add_css_class("detail-panel");
    detail_panel.set_visible(false);
    let detail_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    detail_header.add_css_class("detail-panel-header");
    detail_header.set_margin_start(22);
    detail_header.set_margin_end(14);
    detail_header.set_margin_top(14);
    detail_header.set_margin_bottom(12);
    let detail_heading = gtk::Label::new(Some("Ticket"));
    detail_heading.add_css_class("heading");
    detail_heading.set_xalign(0.0);
    detail_heading.set_hexpand(true);
    detail_header.append(&detail_heading);
    let close_detail = gtk::Button::from_icon_name("window-close-symbolic");
    close_detail.add_css_class("flat");
    close_detail.set_tooltip_text(Some("Close issue details (Esc)"));
    detail_header.append(&close_detail);
    detail_panel.append(&detail_header);
    let detail_box = gtk::Box::new(gtk::Orientation::Vertical, 18);
    detail_box.set_margin_top(20);
    detail_box.set_margin_start(22);
    detail_box.set_margin_end(22);
    detail_box.set_margin_bottom(28);
    let detail_scroll = gtk::ScrolledWindow::new();
    detail_scroll.set_min_content_width(360);
    detail_scroll.set_propagate_natural_width(false);
    detail_scroll.set_child(Some(&detail_box));
    detail_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    detail_scroll.set_vexpand(true);
    detail_panel.append(&detail_scroll);

    // The sidebar exposes real actions and the actual selected project set.
    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 14);
    sidebar.add_css_class("workspace-sidebar");
    sidebar.set_width_request(184);
    sidebar.set_hexpand(false);
    let sidebar_intro = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let eyebrow = gtk::Label::new(Some("WORKSPACE"));
    eyebrow.add_css_class("sidebar-eyebrow");
    eyebrow.set_xalign(0.0);
    sidebar_intro.append(&eyebrow);
    let sidebar_title = gtk::Label::new(Some("Your work"));
    sidebar_title.add_css_class("title-3");
    sidebar_title.set_xalign(0.0);
    sidebar_intro.append(&sidebar_title);
    sidebar.append(&sidebar_intro);

    let board_nav = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    board_nav.add_css_class("sidebar-current");
    board_nav.append(&gtk::Image::from_icon_name("view-grid-symbolic"));
    let nav_label = gtk::Label::new(Some("Unified board"));
    nav_label.set_xalign(0.0);
    nav_label.set_hexpand(true);
    board_nav.append(&nav_label);
    board_nav.append(&manage_projects_button);
    sidebar.append(&board_nav);

    let projects_heading = gtk::Label::new(Some("SELECTED PROJECTS"));
    projects_heading.add_css_class("sidebar-eyebrow");
    projects_heading.set_xalign(0.0);
    projects_heading.set_margin_top(12);
    sidebar.append(&projects_heading);
    let sidebar_projects = gtk::Box::new(gtk::Orientation::Vertical, 7);
    sidebar_projects.add_css_class("sidebar-projects");
    sidebar.append(&sidebar_projects);

    // Rare column configuration stays in the titlebar; projects are changed beside the board.
    let board_menu = gtk::MenuButton::new();
    board_menu.set_icon_name("open-menu-symbolic");
    board_menu.set_tooltip_text(Some("Board settings"));
    board_menu.add_css_class("flat");
    let board_popover = gtk::Popover::new();
    let settings_actions = gtk::Box::new(gtk::Orientation::Vertical, 7);
    settings_actions.add_css_class("board-settings-menu");
    settings_actions.append(&manage_columns_button);
    board_popover.set_child(Some(&settings_actions));
    board_menu.set_popover(Some(&board_popover));
    header.pack_end(&board_menu);

    // A splitter gives the sidebar a draggable edge; the titlebar toggles it.
    let workspace_split = gtk::Paned::new(gtk::Orientation::Horizontal);
    workspace_split.add_css_class("workspace-root");
    workspace_split.set_position(224);
    workspace_split.set_resize_start_child(false);
    workspace_split.set_resize_end_child(true);
    workspace_split.set_shrink_start_child(true);
    workspace_split.set_shrink_end_child(true);
    workspace_split.set_hexpand(true);
    workspace_split.set_start_child(Some(&sidebar));
    let saved_sidebar_width = Rc::new(Cell::new(224));
    sidebar_toggle.connect_toggled({
        let sidebar = sidebar.clone();
        let workspace_split = workspace_split.clone();
        let saved_sidebar_width = saved_sidebar_width.clone();
        move |button| {
            if button.is_active() {
                sidebar.set_visible(true);
                workspace_split.set_position(saved_sidebar_width.get());
            } else {
                let width = workspace_split.position();
                if width >= 184 {
                    saved_sidebar_width.set(width);
                }
                sidebar.set_visible(false);
            }
        }
    });

    let inner_split = gtk::Paned::new(gtk::Orientation::Horizontal);
    inner_split.set_start_child(Some(&center));
    inner_split.set_end_child(Some(&detail_panel));
    // Never let a long description or a wide board force sibling panes off-screen.
    inner_split.set_position(790);
    inner_split.set_resize_start_child(true);
    inner_split.set_resize_end_child(false);
    inner_split.set_shrink_start_child(true);
    inner_split.set_shrink_end_child(true);
    inner_split.set_hexpand(true);
    workspace_split.set_end_child(Some(&inner_split));
    let toast = adw::ToastOverlay::new();
    toast.set_child(Some(&workspace_split));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toast));
    window.set_content(Some(&toolbar));

    let ui = Ui {
        window, toast, header_title, spinner, status, projects_label, sidebar_projects,
        project_filter: Rc::new(RefCell::new(None)),
        connect_button, disconnect_button, refresh_button,
        manage_projects_button, manage_columns_button, search, center_title,
        board_box, detail_box, detail_panel, detail_heading, inner_split,
        saved_detail_width: Rc::new(Cell::new(DETAIL_WIDTH)),
        mine_filter, unassigned_filter, all_filter, sprint_filter,
        image_slots: Rc::new(RefCell::new(issue_detail::ImageSlots::new())),
        events: sender.clone(),
        selected_issue: Rc::new(RefCell::new(None)),
        selected_button: Rc::new(RefCell::new(None)),
        collapsed_columns: Rc::new(RefCell::new(HashSet::new())),
    };
    {
        let panel = ui.detail_panel.downgrade();
        let saved_width = ui.saved_detail_width.clone();
        let selected_issue = ui.selected_issue.clone();
        let selected_button = ui.selected_button.clone();
        close_detail.connect_clicked(move |_| {
            if let Some(panel) = panel.upgrade() {
                close_issue_pane(&panel, &saved_width, &selected_issue, &selected_button);
            }
        });
    }
    {
        let panel = ui.detail_panel.downgrade();
        let search = ui.search.downgrade();
        let saved_width = ui.saved_detail_width.clone();
        let selected_issue = ui.selected_issue.clone();
        let selected_button = ui.selected_button.clone();
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let (Some(panel), Some(search)) = (panel.upgrade(), search.upgrade())
                else { return glib::Propagation::Proceed };
            if key == gtk::gdk::Key::Escape && panel.is_visible() && !search.has_focus() {
                close_issue_pane(&panel, &saved_width, &selected_issue, &selected_button);
                return glib::Propagation::Stop;
            }
            if key == gtk::gdk::Key::slash
                && !search.has_focus()
                && !modifiers.intersects(gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::ALT_MASK)
            {
                search.grab_focus();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        ui.window.add_controller(keys);
    }
    let css = gtk::CssProvider::new();
    css.load_from_string(include_str!("board.css"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
    update_project_label(&ui, &state);
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
        let button = ui.sprint_filter.clone();
        button.connect_toggled(move |button| {
            let active = button.is_active();
            let changed = {
                let mut s = state.borrow_mut();
                if s.active_sprint_only == active { false } else {
                    s.active_sprint_only = active;
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
                s.active_sprint_only = false;
                s.generation
            };
            set_busy(&ui, &state, false);
            ui.header_title.set_subtitle("Not connected");
            ui.status.set_text("Disconnected");
            ui.projects_label.set_text("Choose projects to begin");
            clear_box(&ui.sidebar_projects);
            *ui.project_filter.borrow_mut() = None;
            ui.manage_projects_button.set_sensitive(false);
            ui.manage_columns_button.set_sensitive(false);
            ui.center_title.set_text("My board");
            ui.mine_filter.set_active(true);
            ui.sprint_filter.set_active(false);
            show_empty(&ui.board_box, "Not connected", "Connect to Jira to view issues.");
            close_issue_details(&ui);
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
                        s.active_sprint_only = false;
                    }
                    ui.mine_filter.set_active(true);
            ui.sprint_filter.set_active(false);
                    ui.manage_projects_button.set_sensitive(true);
                    ui.manage_columns_button.set_sensitive(false);
                    ui.header_title.set_subtitle(&loaded.user);
                    update_project_label(ui, state);
                    close_issue_details(ui);
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
                    ui.status.set_text(&format!("{issue_count} issues"));
                    render_board(ui, &state.borrow());
                }
                Err(error) => {
                    show_empty(&ui.board_box, "Could not load project board", &error);
                    show_error(ui, &error);
                }
            }
        }
        Event::ImageLoaded(id, key, attachment_id, result) => {
            if state.borrow().generation != id
                || ui.selected_issue.borrow().as_deref() != Some(key.as_str())
            {
                return;
            }
            issue_detail::set_image(
                &ui.image_slots.borrow(),
                &attachment_id,
                result.as_deref().ok(),
            );
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
    let (id, request, account, projects, scope, active_sprint_only) = {
        let mut s = state.borrow_mut();
        s.board_request += 1;
        (s.generation, s.board_request, s.account.clone(), s.settings.projects.clone(), s.issue_scope, s.active_sprint_only)
    };
    update_project_label(ui, state);
    close_issue_details(ui);
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
    set_busy(ui, state, true);
    let sender = sender.clone();
    thread::spawn(move || {
        let result = (|| -> anyhow::Result<BoardData> {
            let client = JiraClient::new(account)?;
            let statuses = client.project_statuses(&projects)?;
            let issues = client.project_issues(&projects, scope.jql(), active_sprint_only)?;
            Ok(BoardData { statuses, issues })
        })().map_err(|err| err.to_string());
        let _ = sender.send(Event::BoardLoaded(id, request, result));
    });
}

fn update_project_label(ui: &Ui, state: &Rc<RefCell<State>>) {
    let projects = state.borrow().settings.projects.clone();
    clear_box(&ui.sidebar_projects);
    ui.center_title.set_text("My board");

    // The local project filter survives server-side scope and sprint changes.
    if ui.project_filter.borrow().as_ref()
        .is_some_and(|project| !projects.contains(project))
    {
        *ui.project_filter.borrow_mut() = None;
    }
    if projects.is_empty() {
        ui.projects_label.set_text("Choose projects to get started");
        let label = gtk::Label::new(Some("No projects yet"));
        label.add_css_class("dim-label");
        label.set_xalign(0.0);
        ui.sidebar_projects.append(&label);
        return;
    }

    let selected = ui.project_filter.borrow().clone();
    let all_button = gtk::ToggleButton::with_label("All projects");
    all_button.add_css_class("sidebar-project-filter");
    all_button.add_css_class("all-projects");
    all_button.set_active(selected.is_none());
    {
        let ui = ui.clone();
        let state = state.clone();
        all_button.connect_toggled(move |button| {
            if button.is_active() {
                *ui.project_filter.borrow_mut() = None;
                render_board(&ui, &state.borrow());
            }
        });
    }
    ui.sidebar_projects.append(&all_button);

    for project in projects {
        let button = gtk::ToggleButton::new();
        button.set_group(Some(&all_button));
        button.add_css_class("sidebar-project-filter");
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 9);
        let mark = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        mark.add_css_class("sidebar-project-mark");
        row.append(&mark);
        let label = gtk::Label::new(Some(&project));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        row.append(&label);
        button.set_child(Some(&row));
        button.set_active(selected.as_deref() == Some(&project));
        {
            let ui = ui.clone();
            let state = state.clone();
            button.connect_toggled(move |button| {
                if button.is_active() {
                    *ui.project_filter.borrow_mut() = Some(project.clone());
                    render_board(&ui, &state.borrow());
                }
            });
        }
        ui.sidebar_projects.append(&button);
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

// The selector and movement buttons live outside the scrolling column list.
 // A selected status reference identifies its column across reorders.
#[derive(Clone)]
struct ColumnOrderControls {
    selected: Rc<RefCell<Option<preferences::StatusRef>>>,
    updating: Rc<Cell<bool>>,
    selector: gtk::DropDown,
    left: gtk::Button,
    right: gtk::Button,
    scroll: gtk::ScrolledWindow,
}

fn visible_column_indices(settings: &BoardSettings) -> Vec<usize> {
    settings.columns.iter().enumerate()
        .filter(|(_, column)| column.statuses.iter()
            .any(|status| settings.projects.contains(&status.project_key)))
        .map(|(index, _)| index)
        .collect()
}

fn update_column_order_controls(controls: &ColumnOrderControls, state: &Rc<RefCell<State>>) {
    let s = state.borrow();
    let visible = visible_column_indices(&s.settings);
    let selection = controls.selected.borrow().clone();
    let position = selection.as_ref()
        .and_then(|selected| visible.iter().position(|&index|
            s.settings.columns[index].statuses.contains(selected)));
    let position = position.or_else(|| (!visible.is_empty()).then_some(0));
    *controls.selected.borrow_mut() = position
        .and_then(|i| s.settings.columns[visible[i]].statuses.first().cloned());

    let labels: Vec<String> = visible.iter().enumerate()
        .map(|(i, &index)| format!(
            "{}. {}", i + 1,
            column_label(&s.settings.columns[index], &s.settings.columns, &s.settings.projects)
        ))
        .collect();
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let model = gtk::StringList::new(&label_refs);
    controls.updating.set(true);
    controls.selector.set_model(Some(&model));
    controls.selector.set_selected(position.map(|i| i as u32).unwrap_or(gtk::INVALID_LIST_POSITION));
    controls.updating.set(false);
    controls.left.set_sensitive(position.is_some_and(|i| i > 0));
    controls.right.set_sensitive(position.is_some_and(|i| i + 1 < visible.len()));
}

fn scroll_to_selected_column(rows: &gtk::Box, controls: &ColumnOrderControls, state: &Rc<RefCell<State>>) {
    let position = {
        let s = state.borrow();
        let visible = visible_column_indices(&s.settings);
        let selected = controls.selected.borrow();
        selected.as_ref().and_then(|reference| visible.iter()
            .position(|&index| s.settings.columns[index].statuses.contains(reference)))
    };
    let Some(position) = position else { return; };
    let rows = rows.clone();
    let scroll = controls.scroll.clone();
    glib::idle_add_local_once(move || {
        let mut card = rows.first_child();
        for _ in 0..position {
            card = card.and_then(|child| child.next_sibling());
        }
        let Some(card) = card else { return; };
        let Some(bounds) = card.compute_bounds(&rows) else { return; };
        let adjustment = scroll.vadjustment();
        let top = adjustment.value();
        let bottom = top + adjustment.page_size();
        let card_top = f64::from(bounds.y());
        let card_bottom = card_top + f64::from(bounds.height());

        if card_top < top + 12.0 {
            adjustment.set_value((card_top - 12.0).max(0.0));
        } else if card_bottom > bottom - 12.0 {
            adjustment.set_value((card_bottom - adjustment.page_size() + 12.0).max(0.0));
        }
    });
}

fn move_selected_column(
    rows: &gtk::Box,
    ui: &Ui,
    state: &Rc<RefCell<State>>,
    editor: &gtk::Window,
    controls: &ColumnOrderControls,
    left: bool,
) {
    let selection = controls.selected.borrow().clone();
    let Some(selection) = selection else { return; };
    if !state.borrow_mut().settings.move_visible_column(&selection, left) {
        return;
    }
    refresh_column_editor(ui, state, rows, editor, controls);
    scroll_to_selected_column(rows, controls, state);
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
        "Each card is one board column. Select a column below and use the fixed arrows          to move it repeatedly. Add Jira statuses to combine them; Separate splits them."
    ));
    intro.set_wrap(true);
    intro.set_xalign(0.0);
    content.append(&intro);

    let rows = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_child(Some(&rows));

    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let select_label = gtk::Label::new(Some("Move column"));
    toolbar.append(&select_label);
    let selector = gtk::DropDown::from_strings(&[]);
    selector.set_hexpand(true);
    selector.set_tooltip_text(Some("Select the column to move"));
    toolbar.append(&selector);
    let left = gtk::Button::builder().icon_name("go-previous-symbolic").build();
    left.set_tooltip_text(Some("Move selected column left; click repeatedly without moving the mouse"));
    toolbar.append(&left);
    let right = gtk::Button::builder().icon_name("go-next-symbolic").build();
    right.set_tooltip_text(Some("Move selected column right; click repeatedly without moving the mouse"));
    toolbar.append(&right);
    content.append(&toolbar);
    content.append(&scroll);

    let controls = ColumnOrderControls {
        selected: Rc::new(RefCell::new(None)),
        updating: Rc::new(Cell::new(false)),
        selector: selector.clone(),
        left: left.clone(),
        right: right.clone(),
        scroll: scroll.clone(),
    };

    {
        let controls = controls.clone();
        let rows = rows.clone();
        let state = state.clone();
        selector.connect_selected_notify(move |dropdown| {
            if controls.updating.get() { return; }
            let position = dropdown.selected() as usize;
            let s = state.borrow();
            let visible = visible_column_indices(&s.settings);
            let selected = visible.get(position)
                .and_then(|&index| s.settings.columns[index].statuses.first().cloned());
            let can_move = visible.get(position).is_some();
            controls.left.set_sensitive(can_move && position > 0);
            controls.right.set_sensitive(can_move && position + 1 < visible.len());
            drop(s);
            *controls.selected.borrow_mut() = selected;
            scroll_to_selected_column(&rows, &controls, &state);
        });
    }
    {
        let controls = controls.clone();
        let ui = ui.clone();
        let state = state.clone();
        let rows = rows.clone();
        let editor = dialog.clone();
        left.connect_clicked(move |_| {
            move_selected_column(&rows, &ui, &state, &editor, &controls, true);
        });
    }
    {
        let controls = controls.clone();
        let ui = ui.clone();
        let state = state.clone();
        let rows = rows.clone();
        let editor = dialog.clone();
        right.connect_clicked(move |_| {
            move_selected_column(&rows, &ui, &state, &editor, &controls, false);
        });
    }
    rebuild_editor(&rows, ui, state, &dialog, &controls);

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
        let controls = controls.clone();
        reset.connect_clicked(move |_| {
            confirm_column_reset(&ui, &state, &rows, &editor, &controls);
        });
    }
    dialog.present();
}

fn rebuild_editor(
    rows: &gtk::Box,
    ui: &Ui,
    state: &Rc<RefCell<State>>,
    editor: &gtk::Window,
    controls: &ColumnOrderControls,
) {
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

        // Selecting the card only changes the selection in the persistent toolbar.
        // Moving is exclusively handled by the toolbar's fixed-position arrows.
        let choose = gtk::Button::with_label("Select");
        choose.set_tooltip_text(Some("Select this column in the fixed move toolbar"));
        header.append(&choose);
        {
            let controls = controls.clone();
            let state = state.clone();
            let reference = column.statuses[0].clone();
            choose.connect_clicked(move |_| {
                *controls.selected.borrow_mut() = Some(reference.clone());
                update_column_order_controls(&controls, &state);
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
            let controls = controls.clone();
            rename.connect_clicked(move |_| {
                let renamed = {
                    let mut s = state.borrow_mut();
                    let index = s.settings.columns.iter()
                        .position(|column| column.statuses.contains(&anchor));
                    index.is_some_and(|index| s.settings.rename_column(index, &name.text()))
                };
                if renamed {
                    refresh_column_editor(&ui, &state, &rows, &editor, &controls);
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
                let controls = controls.clone();
                separate.connect_clicked(move |_| {
                    state.borrow_mut().settings.move_status(
                        &status.reference(), None, &status.name
                    );
                    refresh_column_editor(&ui, &state, &rows, &editor, &controls);
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
            let controls = controls.clone();
            add.connect_clicked(move |_| {
                let target = {
                    let s = state.borrow();
                    s.settings.columns.iter()
                        .position(|column| column.statuses.contains(&anchor))
                };
                if let Some(target) = target {
                    show_add_status_picker(&ui, &state, &rows, &editor, target, &controls);
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
    update_column_order_controls(controls, state);
}

fn show_add_status_picker(
    ui: &Ui,
    state: &Rc<RefCell<State>>,
    rows: &gtk::Box,
    editor: &gtk::Window,
    target: usize,
    controls: &ColumnOrderControls,
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
        let controls = controls.clone();
        add.connect_clicked(move |_| {
            state.borrow_mut().settings.move_status(
                &status.reference(), Some(target), &status.name
            );
            refresh_column_editor(&ui, &state, &rows, &editor, &controls);
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
    controls: &ColumnOrderControls,
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
        "Restore suggested column order and default status groups? Jira statuses \
         with matching names will be grouped automatically. Your selected projects \
         will be kept."
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
        let controls = controls.clone();
        reset.connect_clicked(move |_| {
            let statuses = state.borrow().statuses.clone();
            state.borrow_mut().settings.reset_columns(&statuses);
            refresh_column_editor(&ui, &state, &rows, &editor, &controls);
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
    controls: &ColumnOrderControls,
) {
    save_settings(ui, state);
    render_board(ui, &state.borrow());
    rebuild_editor(rows, ui, state, editor, controls);
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

fn visible_board_groups<'a>(
    settings: &BoardSettings,
    issues: &'a [Issue],
    search: &str,
) -> Vec<(String, Vec<&'a Issue>)> {
    let text = search.to_lowercase();
    let mut groups: Vec<(String, Vec<&Issue>, Vec<preferences::StatusRef>)> = settings.columns.iter()
        .filter(|column| column.statuses.iter()
            .any(|status| settings.projects.contains(&status.project_key)))
        .map(|column| (
            column_label(column, &settings.columns, &settings.projects),
            Vec::new(),
            column.statuses.clone(),
        ))
        .collect();

    for issue in issues {
        // Epics organize work; only their child issues should appear as cards.
        if issue.is_epic {
            continue;
        }
        if !text.is_empty()
            && !issue.key.to_lowercase().contains(&text)
            && !issue.summary.to_lowercase().contains(&text)
            && !issue.status.to_lowercase().contains(&text)
            && !issue.epic.as_ref().is_some_and(|epic| {
                epic.key.to_lowercase().contains(&text)
                    || epic.summary.to_lowercase().contains(&text)
            })
        {
            continue;
        }
        let reference = preferences::StatusRef {
            project_key: issue.project_key.clone(),
            status_id: issue.status_id.clone(),
        };
        if let Some((_, matches, _)) = groups.iter_mut()
            .find(|(_, _, statuses)| statuses.contains(&reference))
        {
            matches.push(issue);
        } else if let Some((_, matches, _)) = groups.iter_mut()
            .find(|(title, _, _)| title == "Other")
        {
            matches.push(issue);
        } else {
            groups.push(("Other".into(), vec![issue], Vec::new()));
        }
    }

    // Empty columns remain in board settings and the column editor.
    // Only the current scope/search result determines what is rendered.
    groups.into_iter()
        .filter_map(|(name, matches, _)| (!matches.is_empty()).then_some((name, matches)))
        .collect()
}


fn close_issue_details(ui: &Ui) {
    close_issue_pane(
        &ui.detail_panel,
        &ui.saved_detail_width,
        &ui.selected_issue,
        &ui.selected_button,
    );
}

fn close_issue_pane(
    panel: &gtk::Box,
    saved_width: &Cell<i32>,
    selected_issue: &Rc<RefCell<Option<String>>>,
    selected_button: &Rc<RefCell<Option<gtk::Button>>>,
) {
    // GTK resets hidden Paned children; preserve the actual user-resized width first.
    if panel.is_visible() && panel.width() >= 320 {
        saved_width.set(panel.width());
    }
    panel.set_visible(false);
    *selected_issue.borrow_mut() = None;
    if let Some(button) = selected_button.borrow_mut().take() {
        button.remove_css_class("selected-issue");
    }
}

fn filter_project_groups(groups: &mut Vec<(String, Vec<&Issue>)>, project: Option<&str>) {
    if let Some(project) = project {
        for (_, issues) in groups.iter_mut() {
            issues.retain(|issue| issue.project_key == project);
        }
        groups.retain(|(_, issues)| !issues.is_empty());
    }
}

fn render_board(ui: &Ui, state: &State) {
    clear_box(&ui.board_box);
    *ui.selected_button.borrow_mut() = None;
    if state.settings.projects.is_empty() {
        close_issue_details(ui);
        show_empty(&ui.board_box, "Choose projects", "Use Projects… to set up your board.");
        return;
    }
    let search = ui.search.text();
    let mut groups = visible_board_groups(&state.settings, &state.issues, search.as_str());
    filter_project_groups(&mut groups, ui.project_filter.borrow().as_deref());
    let shown = groups.iter().map(|(_, issues)| issues.len()).sum::<usize>();
    if ui.project_filter.borrow().is_some() || !search.is_empty() {
        ui.status.set_text(&format!("{shown} shown"));
    } else {
        ui.status.set_text(&format!("{} issues", state.issues.len()));
    }
    if let Some(project) = ui.project_filter.borrow().as_deref() {
        ui.projects_label.set_text(&format!("{project} · filtered"));
    } else if !state.settings.projects.is_empty() {
        ui.projects_label.set_text(&format!("Across {}", state.settings.projects.join(" · ")));
    }
    if groups.is_empty() {
        close_issue_details(ui);
        show_empty(&ui.board_box, "No matching issues", "No issues match the current filter or search.");
        return;
    }
    let selected_key = ui.selected_issue.borrow().clone();
    if let Some(key) = selected_key
        && !groups.iter().any(|(_, issues)| issues.iter().any(|issue| issue.key == key))
    {
        close_issue_details(ui);
    }

    let url = state.account.as_ref().map(|a| a.base_url.as_str()).unwrap_or("");
    for (name, issues) in groups {
        let issue_count = issues.len();
        let initially_collapsed = ui.collapsed_columns.borrow().contains(&name);
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.add_css_class("board-column");
        column.add_css_class(&format!("column-tone-{}", epic_color_index(&name) % 5));
        if initially_collapsed {
            column.add_css_class("collapsed-column");
        }
        column.set_width_request(if initially_collapsed { COLLAPSED_COLUMN_WIDTH } else { COLUMN_WIDTH });
        column.set_hexpand(false);
        column.set_vexpand(true);

        let expanded = gtk::Box::new(gtk::Orientation::Vertical, 8);
        expanded.set_visible(!initially_collapsed);
        expanded.set_vexpand(true);
        let heading = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        heading.add_css_class("board-column-header");
        let title = gtk::Label::new(Some(&name));
        title.add_css_class("board-column-title");
        title.set_xalign(0.0);
        title.set_hexpand(true);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        heading.append(&title);
        let count = gtk::Label::new(Some(&issue_count.to_string()));
        count.add_css_class("board-column-count");
        heading.append(&count);
        let collapse = gtk::Button::from_icon_name("pan-start-symbolic");
        collapse.add_css_class("flat");
        collapse.add_css_class("column-collapse");
        collapse.set_tooltip_text(Some(&format!("Collapse {name}")));
        heading.append(&collapse);
        expanded.append(&heading);

        let item_box = gtk::Box::new(gtk::Orientation::Vertical, 7);
        for issue in issues {
            item_box.append(&issue_button(issue, ui, url, state.account.as_ref(), state.generation));
        }
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_child(Some(&item_box));
        scroll.set_vexpand(true);
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        expanded.append(&scroll);
        column.append(&expanded);

        let collapsed = gtk::Box::new(gtk::Orientation::Vertical, 0);
        collapsed.set_visible(initially_collapsed);
        collapsed.set_vexpand(true);
        let expand = gtk::Button::new();
        expand.add_css_class("flat");
        expand.add_css_class("collapsed-expand");
        expand.set_hexpand(true);
        expand.set_tooltip_text(Some(&format!("Expand {name}")));
        let collapsed_content = gtk::Box::new(gtk::Orientation::Vertical, 9);
        collapsed_content.set_halign(gtk::Align::Center);
        let expand_icon = gtk::Image::from_icon_name("pan-end-symbolic");
        collapsed_content.append(&expand_icon);
        let collapsed_title = gtk::Label::new(Some(&name));
        collapsed_title.add_css_class("collapsed-title");
        collapsed_title.set_wrap(true);
        collapsed_title.set_justify(gtk::Justification::Center);
        collapsed_title.set_max_width_chars(12);
        collapsed_title.set_xalign(0.5);
        collapsed_content.append(&collapsed_title);
        let collapsed_count = gtk::Label::new(Some(&issue_count.to_string()));
        collapsed_count.add_css_class("collapsed-count");
        collapsed_content.append(&collapsed_count);
        expand.set_child(Some(&collapsed_content));
        collapsed.append(&expand);
        column.append(&collapsed);

        {
            let collapsed_columns = ui.collapsed_columns.clone();
            let name = name.clone();
            let column = column.downgrade();
            let expanded = expanded.downgrade();
            let collapsed = collapsed.downgrade();
            collapse.connect_clicked(move |_| {
                let (Some(column), Some(expanded), Some(collapsed)) =
                    (column.upgrade(), expanded.upgrade(), collapsed.upgrade()) else { return };
                collapsed_columns.borrow_mut().insert(name.clone());
                expanded.set_visible(false);
                collapsed.set_visible(true);
                column.set_width_request(COLLAPSED_COLUMN_WIDTH);
                column.add_css_class("collapsed-column");
            });
        }
        {
            let collapsed_columns = ui.collapsed_columns.clone();
            let column = column.downgrade();
            let expanded = expanded.downgrade();
            let collapsed = collapsed.downgrade();
            expand.connect_clicked(move |_| {
                let (Some(column), Some(expanded), Some(collapsed)) =
                    (column.upgrade(), expanded.upgrade(), collapsed.upgrade()) else { return };
                collapsed_columns.borrow_mut().remove(&name);
                collapsed.set_visible(false);
                expanded.set_visible(true);
                column.set_width_request(COLUMN_WIDTH);
                column.remove_css_class("collapsed-column");
            });
        }
        ui.board_box.append(&column);
    }
}

// Fixed FNV-1a hashing keeps an Epic's accent stable across launches and scopes.
fn epic_color_index(key: &str) -> u32 {
    key.bytes().fold(0x811c9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x01000193)
    }) % 8
}

fn issue_button(issue: &Issue, ui: &Ui, base_url: &str, account: Option<&Credentials>, generation: u64) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("issue-card");
    button.set_hexpand(true);
    button.set_vexpand(false);
    button.set_tooltip_text(Some(&format!("{} · {}", issue.key, issue.summary)));
    let surface = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    if let Some(epic) = &issue.epic {
        let stripe = gtk::Box::new(gtk::Orientation::Vertical, 0);
        stripe.add_css_class("epic-stripe");
        stripe.add_css_class(&format!("epic-color-{}", epic_color_index(&epic.key)));
        // Fill the card height without requesting vertical expansion in GtkBox.
        stripe.set_valign(gtk::Align::Fill);
        stripe.set_vexpand(false);
        surface.append(&stripe);
    }
    let content = gtk::Box::new(gtk::Orientation::Vertical, 7);
    content.set_hexpand(true);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);

    let meta = gtk::Box::new(gtk::Orientation::Horizontal, 7);
    let kind = issue.kind.to_ascii_lowercase();
    let (kind_icon, kind_class) = if kind.contains("bug") {
        ("dialog-warning-symbolic", "kind-bug")
    } else if kind.contains("story") {
        ("starred-symbolic", "kind-story")
    } else {
        ("view-list-symbolic", "kind-task")
    };
    let type_mark = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    type_mark.add_css_class("ticket-type-mark");
    type_mark.add_css_class(kind_class);
    type_mark.append(&gtk::Image::from_icon_name(kind_icon));
    meta.append(&type_mark);
    let key = gtk::Label::new(Some(&issue.key));
    key.add_css_class("ticket-key");
    key.set_hexpand(true);
    key.set_xalign(0.0);
    meta.append(&key);
    if !issue.kind.is_empty() {
        let kind = gtk::Label::new(Some(&issue.kind));
        kind.add_css_class("ticket-kind");
        meta.append(&kind);
    }
    let priority = issue.priority.to_ascii_lowercase();
    if matches!(priority.as_str(), "highest" | "high" | "critical" | "blocker") {
        let badge = gtk::Label::new(Some(&issue.priority));
        badge.add_css_class("ticket-priority");
        meta.append(&badge);
    }
    content.append(&meta);
    let summary = gtk::Label::new(Some(&issue.summary));
    summary.set_xalign(0.0);
    summary.set_wrap(true);
    summary.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    summary.set_max_width_chars(38);
    summary.set_lines(4);
    summary.set_ellipsize(gtk::pango::EllipsizeMode::End);
    summary.add_css_class("ticket-summary");
    content.append(&summary);

    if let Some(epic) = &issue.epic {
        let epic_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        epic_row.add_css_class("ticket-epic");
        let icon = gtk::Image::from_icon_name("view-grid-symbolic");
        icon.add_css_class("ticket-epic-icon");
        epic_row.append(&icon);
        let title = if epic.summary.is_empty() { &epic.key } else { &epic.summary };
        let label = gtk::Label::new(None);
        label.add_css_class("ticket-epic-label");
        if base_url.is_empty() {
            label.set_text(title);
        } else {
            let url = format!("{base_url}/browse/{}", epic.key);
            label.set_markup(&format!(
                "<a href=\"{}\">{}</a>",
                glib::markup_escape_text(&url),
                glib::markup_escape_text(title),
            ));
        }
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_tooltip_text(Some(&format!("{} · {}", epic.key, title)));
        epic_row.append(&label);
        content.append(&epic_row);
    }
    surface.append(&content);
    button.set_child(Some(&surface));

    if ui.selected_issue.borrow().as_deref() == Some(issue.key.as_str()) {
        button.add_css_class("selected-issue");
        *ui.selected_button.borrow_mut() = Some(button.clone());
    }

    let issue = issue.clone();
    let detail_box = ui.detail_box.clone();
    let detail_panel = ui.detail_panel.clone();
    let inner_split = ui.inner_split.clone();
    let detail_heading = ui.detail_heading.clone();
    let saved_detail_width = ui.saved_detail_width.clone();
    let selected_button = Rc::downgrade(&ui.selected_button);
    let selected_issue = ui.selected_issue.clone();
    let base_url = base_url.to_owned();
    let account = account.cloned();
    let image_slots = ui.image_slots.clone();
    let events = ui.events.clone();
    let toast = ui.toast.clone();
    button.connect_clicked(move |button| {
        if let Some(selected_button) = selected_button.upgrade()
            && let Some(previous) = selected_button.borrow_mut().replace(button.clone())
        {
            previous.remove_css_class("selected-issue");
        }
        button.add_css_class("selected-issue");
        *selected_issue.borrow_mut() = Some(issue.key.clone());
        detail_heading.set_text(&issue.key);
        *image_slots.borrow_mut() = issue_detail::render(&detail_box, &issue, &base_url, &toast);
        if !detail_panel.is_visible() {
            detail_panel.set_visible(true);
            let available = inner_split.width();
            if available >= 640 {
                let width = saved_detail_width.get().min(available - 300).max(320);
                inner_split.set_position(available - width);
            }
        }
        if let Some(account) = account.clone() {
            let attachments: Vec<_> = issue.attachments.iter()
                .filter(|a| a.is_image() && !a.content_url.is_empty()).cloned().collect();
            if !attachments.is_empty() {
                let events = events.clone();
                let key = issue.key.clone();
                thread::spawn(move || {
                    let Ok(client) = JiraClient::new(account) else { return };
                    for attachment in attachments {
                        let result = client.image_bytes(&attachment).map_err(|e| e.to_string());
                        let _ = events.send(Event::ImageLoaded(
                            generation, key.clone(), attachment.id, result
                        ));
                    }
                });
            }
        }
    });
    button
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

    fn issue(key: &str, status_id: &str, summary: &str) -> Issue {
        Issue {
            key: key.into(),
            project_key: "ART".into(),
            summary: summary.into(),
            status: format!("Status {status_id}"),
            status_id: status_id.into(),
            kind: String::new(),
            priority: String::new(),
            assignee: String::new(),
            updated: String::new(),
            description: String::new(),
            description_doc: None,
            attachments: Vec::new(),
            is_epic: false,
            epic: None,
        }
    }

    fn board_settings() -> BoardSettings {
        BoardSettings {
            projects: vec!["ART".into()],
            columns: ["Open", "Review", "Done"].iter().enumerate().map(|(i, title)| {
                preferences::Column {
                    title: (*title).into(),
                    statuses: vec![preferences::StatusRef {
                        project_key: "ART".into(),
                        status_id: (i + 1).to_string(),
                    }],
                }
            }).collect(),
            ..BoardSettings::default()
        }
    }

    #[test]
    fn hides_zero_issue_columns_but_keeps_their_configuration() {
        let settings = board_settings();
        let issues = vec![
            issue("ART-1", "1", "Relevant issue"),
            issue("ART-2", "3", "Completed issue"),
        ];
        let groups = visible_board_groups(&settings, &issues, "");
        assert_eq!(
            groups.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            vec!["Open", "Done"],
        );
        assert_eq!(settings.columns.len(), 3);
    }

    #[test]
    fn hides_columns_that_become_empty_after_search_or_scope_filter() {
        let settings = board_settings();
        // The issues slice already represents the server-filtered Mine/Unassigned/All scope.
        let issues = vec![
            issue("ART-1", "1", "Relevant issue"),
            issue("ART-2", "3", "Completed issue"),
        ];
        let groups = visible_board_groups(&settings, &issues, "Relevant");
        assert_eq!(
            groups.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            vec!["Open"],
        );
        assert!(visible_board_groups(&settings, &issues, "no matches").is_empty());
        assert!(visible_board_groups(&settings, &[], "").is_empty());
    }

    #[test]
    fn keeps_other_column_only_when_unmapped_issues_exist() {
        let settings = board_settings();
        let issues = vec![issue("ART-404", "other", "Unmapped")];
        let groups = visible_board_groups(&settings, &issues, "");
        assert_eq!(
            groups.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            vec!["Other"],
        );
    }

    #[test]
    fn epic_cards_are_hidden_but_children_keep_their_place() {
        let settings = board_settings();
        let mut epic = issue("ART-1", "1", "Improve questionnaire");
        epic.kind = "Epic".into();
        epic.is_epic = true;
        let mut bug = issue("ART-2", "1", "Fix validation");
        bug.epic = Some(crate::jira::Epic {
            key: "ART-1".into(),
            summary: epic.summary.clone(),
        });
        let issues = [epic, bug];
        let groups = visible_board_groups(&settings, &issues, "");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].1.len(), 1);
        assert_eq!(groups[0].1[0].key, "ART-2");
    }

    #[test]
    fn searches_issues_by_epic_title_or_key() {
        let settings = board_settings();
        let mut bug = issue("ART-2", "1", "Fix validation");
        bug.epic = Some(crate::jira::Epic {
            key: "ART-1".into(),
            summary: "Questionnaire redesign".into(),
        });
        let issues = [bug];
        assert_eq!(visible_board_groups(&settings, &issues, "redesign").len(), 1);
        assert_eq!(visible_board_groups(&settings, &issues, "ART-1").len(), 1);
        assert!(visible_board_groups(&settings, &issues, "different").is_empty());
    }

    #[test]
    fn project_filter_keeps_only_the_selected_project_and_hides_empty_columns() {
        let art = issue("ART-1", "1", "Work for ART");
        let mut afb = issue("AFB-2", "2", "Work for AFB");
        afb.project_key = "AFB".into();
        let issues = [art, afb];
        let mut groups = vec![
            ("Open".into(), vec![&issues[0]]),
            ("Review".into(), vec![&issues[1]]),
        ];
        filter_project_groups(&mut groups, Some("AFB"));
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, "Review");
        assert_eq!(groups[0].1[0].key, "AFB-2");

        filter_project_groups(&mut groups, None);
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn epic_accent_is_deterministic() {
        assert_eq!(epic_color_index("ART-123"), epic_color_index("ART-123"));
        assert!(epic_color_index("ART-123") < 8);
    }

    #[test]
    fn board_scope_uses_server_side_jql() {
        assert_eq!(IssueScope::Mine.jql(), Some("assignee = currentUser()"));
        assert_eq!(IssueScope::Unassigned.jql(), Some("assignee IS EMPTY"));
        assert_eq!(IssueScope::All.jql(), None);
    }
}
