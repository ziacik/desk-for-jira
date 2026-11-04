use std::{
    collections::HashSet,
    cell::{Cell, RefCell},
    rc::Rc,
    sync::mpsc::{self, Sender},
    thread,
    time::Duration,
};

use adw::prelude::*;
use gtk::glib;

use crate::{
    jira::{AuthMode, Board, BoardColumn, Credentials, Issue, JiraClient},
    preferences, secrets,
};

#[derive(Clone)]
struct Ui {
    window: adw::ApplicationWindow,
    toast: adw::ToastOverlay,
    header_title: adw::WindowTitle,
    spinner: gtk::Spinner,
    status: gtk::Label,
    my_button: gtk::Button,
    connect_button: gtk::Button,
    disconnect_button: gtk::Button,
    refresh_button: gtk::Button,
    boards_box: gtk::Box,
    manage_boards_button: gtk::Button,
    search: gtk::SearchEntry,
    center_title: gtk::Label,
    stack: gtk::Stack,
    issue_box: gtk::Box,
    board_box: gtk::Box,
    board_stack: gtk::Stack,
    board_switcher: gtk::StackSwitcher,
    mine_filter: gtk::ToggleButton,
    unassigned_filter: gtk::ToggleButton,
    all_filter: gtk::ToggleButton,
    backlog_box: gtk::Box,
    detail_box: gtk::Box,
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
    issues: Vec<Issue>,
    boards: Vec<Board>,
    pinned_boards: Vec<u64>,
    selected_board: Option<u64>,
    issue_scope: IssueScope,
    board_request: u64,
    generation: u64,
    busy: bool,
}

struct Loaded {
    user: String,
    issues: Vec<Issue>,
    boards: Result<Vec<Board>, String>,
}

struct BoardData {
    issues: Vec<Issue>,
    columns: Vec<BoardColumn>,
    backlog: Option<Result<Vec<Issue>, String>>,
    is_kanban: bool,
}

enum Event {
    Stored(Result<Option<Credentials>, String>),
    Loaded(u64, Credentials, Result<Loaded, String>),
    BoardLoaded(u64, u64, u64, Result<BoardData, String>),
    Disconnected(u64, Result<(), String>),
}

pub fn run() -> glib::ExitCode {
    let application = adw::Application::builder()
        .application_id("com.github.ziacik.DeskForJira")
        .build();
    application.connect_activate(build_ui);
    application.run()
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
    let header_title = adw::WindowTitle::new("Desk for Jira", "Jira issues & boards");
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

    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 8);
    sidebar.set_width_request(238);
    sidebar.set_margin_top(18);
    sidebar.set_margin_bottom(14);
    sidebar.set_margin_start(14);
    sidebar.set_margin_end(14);
    sidebar.add_css_class("sidebar");

    let my_button = gtk::Button::with_label("My issues");
    my_button.set_tooltip_text(Some("Issues assigned to me"));
    my_button.add_css_class("suggested-action");
    sidebar.append(&my_button);

    let boards_label = gtk::Label::new(Some("PINNED BOARDS"));
    boards_label.add_css_class("dim-label");
    boards_label.add_css_class("heading");
    boards_label.set_xalign(0.0);
    boards_label.set_margin_top(18);
    sidebar.append(&boards_label);

    let boards_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let boards_scroll = gtk::ScrolledWindow::new();
    boards_scroll.set_vexpand(true);
    boards_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    boards_scroll.set_child(Some(&boards_box));
    sidebar.append(&boards_scroll);

    let manage_boards_button = gtk::Button::with_label("Manage boards…");
    manage_boards_button.set_tooltip_text(Some("Choose which boards appear in the sidebar"));
    manage_boards_button.set_sensitive(false);
    sidebar.append(&manage_boards_button);

    let status = gtk::Label::new(Some("Connecting to keyring…"));
    status.add_css_class("dim-label");
    status.set_xalign(0.0);
    status.set_wrap(true);
    sidebar.append(&status);

    let center = gtk::Box::new(gtk::Orientation::Vertical, 8);
    center.set_margin_top(16);
    center.set_margin_start(16);
    center.set_margin_end(12);
    center.set_margin_bottom(12);
    center.set_hexpand(true);
    let center_title = gtk::Label::new(Some("My issues"));
    center_title.add_css_class("title-2");
    center_title.set_xalign(0.0);
    center.append(&center_title);

    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Filter by key, title or status"));
    center.append(&search);

    let stack = gtk::Stack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    let issue_box = gtk::Box::new(gtk::Orientation::Vertical, 5);
    let issue_scroll = gtk::ScrolledWindow::new();
    issue_scroll.set_child(Some(&issue_box));
    issue_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    stack.add_named(&issue_scroll, Some("issues"));

    let board_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    board_box.set_halign(gtk::Align::Start);
    let board_scroll = gtk::ScrolledWindow::new();
    board_scroll.set_child(Some(&board_box));
    board_scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Never);

    let backlog_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    backlog_box.set_margin_top(4);
    backlog_box.set_margin_end(12);
    let backlog_scroll = gtk::ScrolledWindow::new();
    backlog_scroll.set_child(Some(&backlog_box));
    backlog_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);

    let board_stack = gtk::Stack::new();
    board_stack.set_vexpand(true);
    board_stack.add_titled(&board_scroll, Some("board"), "Board");
    board_stack.add_titled(&backlog_scroll, Some("backlog"), "Backlog");

    let board_switcher = gtk::StackSwitcher::new();
    board_switcher.set_stack(Some(&board_stack));
    board_switcher.set_halign(gtk::Align::Start);
    board_switcher.set_visible(false);

    let mine_filter = gtk::ToggleButton::with_label("Mine");
    let unassigned_filter = gtk::ToggleButton::with_label("Unassigned");
    let all_filter = gtk::ToggleButton::with_label("All");
    unassigned_filter.set_group(Some(&mine_filter));
    all_filter.set_group(Some(&mine_filter));
    mine_filter.set_active(true);
    for button in [&mine_filter, &unassigned_filter, &all_filter] {
        button.set_tooltip_text(Some("Filter issues by assignee"));
    }
    let filter_group = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    filter_group.add_css_class("linked");
    filter_group.append(&mine_filter);
    filter_group.append(&unassigned_filter);
    filter_group.append(&all_filter);

    let board_controls = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    board_controls.append(&board_switcher);
    board_controls.append(&filter_group);

    let board_page = gtk::Box::new(gtk::Orientation::Vertical, 8);
    board_page.set_vexpand(true);
    board_page.append(&board_controls);
    board_page.append(&board_stack);
    stack.add_named(&board_page, Some("board"));
    center.append(&stack);

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
        window,
        toast,
        header_title,
        spinner,
        status,
        my_button,
        connect_button,
        disconnect_button,
        refresh_button,
        boards_box,
        manage_boards_button,
        search,
        center_title,
        stack,
        issue_box,
        board_box,
        board_stack,
        board_switcher,
        mine_filter,
        unassigned_filter,
        all_filter,
        backlog_box,
        detail_box,
    };

    show_detail_placeholder(&ui.detail_box);
    show_empty(&ui.issue_box, "Connect a Jira account", "Your assigned issues will appear here.");
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
        let result = secrets::load().map_err(|err| err.to_string());
        let _ = sender.send(Event::Stored(result));
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
            if !button.is_active() {
                return;
            }
            let board = {
                let mut s = state.borrow_mut();
                if s.issue_scope == scope {
                    return;
                }
                s.issue_scope = scope;
                s.selected_board
                    .and_then(|id| s.boards.iter().find(|board| board.id == id).cloned())
            };
            if let Some(board) = board {
                start_board_load(&ui, &state, &sender, board);
            }
        });
    }
    {
        let ui = ui.clone();
        let state = state.clone();
        let sender = sender.clone();
        let button = ui.manage_boards_button.clone();
        button.connect_clicked(move |_| show_board_picker(&ui, &state, &sender));
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
            if let Some(account) = state.borrow().account.clone() {
                start_refresh(&ui, &state, &sender, account, false);
            } else {
                show_login(&ui, &state, &sender);
            }
        });
    }
    {
        let ui = ui.clone();
        let state = state.clone();
        let button = ui.my_button.clone();
        button.connect_clicked(move |_| {
            {
                let mut s = state.borrow_mut();
                s.selected_board = None;
                s.board_request += 1;
            }
            set_busy(&ui, &state, false);
            ui.stack.set_visible_child_name("issues");
            ui.search.set_visible(true);
            ui.center_title.set_text("My issues");
            render_my_issues(&ui, &state.borrow());
        });
    }
    {
        let ui = ui.clone();
        let state = state.clone();
        let search = ui.search.clone();
        search.connect_search_changed(move |_| {
            if state.borrow().selected_board.is_none() {
                render_my_issues(&ui, &state.borrow());
            }
        });
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
                s.account = None;
                s.issues.clear();
                s.boards.clear();
                s.pinned_boards.clear();
                s.selected_board = None;
                s.board_request += 1;
                s.issue_scope = IssueScope::Mine;
                s.busy = false;
                s.generation
            };
            ui.header_title.set_subtitle("Not connected");
            ui.status.set_text("Disconnected");
            ui.spinner.stop();
            ui.spinner.set_visible(false);
            clear_box(&ui.boards_box);
            ui.manage_boards_button.set_sensitive(false);
            ui.board_switcher.set_visible(false);
            ui.mine_filter.set_active(true);
            clear_box(&ui.backlog_box);
            ui.center_title.set_text("My issues");
            ui.stack.set_visible_child_name("issues");
            ui.search.set_visible(true);
            show_empty(&ui.issue_box, "Not connected", "Connect to Jira to view issues.");
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
            if state.borrow().generation != id {
                return;
            }
            set_busy(ui, state, false);
            match result {
                Ok(loaded) => {
                    let pinned = match preferences::load_pinned(&account.base_url) {
                        Ok(pinned) => pinned,
                        Err(error) => {
                            show_error(ui, &format!("Could not load pinned boards: {error}"));
                            Vec::new()
                        }
                    };
                    {
                        let mut s = state.borrow_mut();
                        s.account = Some(account);
                        s.issues = loaded.issues;
                        s.boards = loaded.boards.as_ref().cloned().unwrap_or_default();
                        s.pinned_boards = pinned;
                        s.selected_board = None;
                        s.board_request += 1;
                        s.issue_scope = IssueScope::Mine;
                    }
                    ui.mine_filter.set_active(true);
                    ui.manage_boards_button.set_sensitive(true);
                    ui.header_title.set_subtitle(&loaded.user);
                    ui.status.set_text(&format!("{} assigned issues · {} boards",
                        state.borrow().issues.len(), state.borrow().boards.len()));
                    ui.search.set_visible(true);
                    ui.stack.set_visible_child_name("issues");
                    ui.center_title.set_text("My issues");
                    render_my_issues(ui, &state.borrow());
                    render_board_list(ui, state, sender);
                    show_detail_placeholder(&ui.detail_box);
                    if let Err(error) = loaded.boards {
                        show_error(ui, &format!("Boards could not be loaded: {error}"));
                    }
                }
                Err(error) => {
                    ui.status.set_text("Connection failed");
                    show_error(ui, &error);
                }
            }
        }
        Event::BoardLoaded(id, board_id, request_id, result) => {
            if state.borrow().generation != id
                || state.borrow().selected_board != Some(board_id)
                || state.borrow().board_request != request_id
            {
                return;
            }
            set_busy(ui, state, false);
            match result {
                Ok(data) => {
                    let base_url = state.borrow().account.as_ref()
                        .map(|a| a.base_url.clone()).unwrap_or_default();

                    // The board endpoint can include backlog issues. Do not display
                    // the same ticket in both the Kanban board and its backlog.
                    let backlog_keys: HashSet<&str> = data.backlog.as_ref()
                        .and_then(|result| result.as_ref().ok())
                        .into_iter()
                        .flatten()
                        .map(|issue| issue.key.as_str())
                        .collect();
                    let board_issues: Vec<Issue> = data.issues.into_iter()
                        .filter(|issue| !backlog_keys.contains(issue.key.as_str()))
                        .collect();
                    let board_count = board_issues.len();
                    render_board(ui, board_issues, data.columns, &base_url, data.is_kanban);

                    match data.backlog {
                        Some(Ok(backlog)) => {
                            ui.status.set_text(&format!(
                                "{} board issues · {} backlog issues", board_count, backlog.len()
                            ));
                            render_backlog(ui, &backlog, &base_url);
                        }
                        Some(Err(error)) => {
                            ui.status.set_text(&format!("{} issues on this board", board_count));
                            show_empty(&ui.backlog_box, "Backlog unavailable", &error);
                        }
                        None => {
                            ui.status.set_text(&format!("{} issues on this board", board_count));
                        }
                    }
                }
                Err(error) => {
                    show_empty(&ui.board_box, "Could not load this board", &error);
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
        s.selected_board = None;
        s.generation
    };
    set_busy(ui, state, true);
    ui.status.set_text("Loading Jira…");
    ui.stack.set_visible_child_name("issues");
    ui.center_title.set_text("My issues");
    ui.search.set_visible(true);
    let sender = sender.clone();
    thread::spawn(move || {
        let result = (|| -> anyhow::Result<Loaded> {
            let client = JiraClient::new(account.clone())?;
            let user = client.myself()?;
            let issues = client.my_issues()?;
            let boards = client.boards().map_err(|err| err.to_string());
            if save {
                secrets::save(&account)?;
            }
            Ok(Loaded { user, issues, boards })
        })().map_err(|err| err.to_string());
        let _ = sender.send(Event::Loaded(id, account, result));
    });
}

fn start_board_load(ui: &Ui, state: &Rc<RefCell<State>>, sender: &Sender<Event>, board: Board) {
    let (id, request_id, account, scope) = {
        let mut s = state.borrow_mut();
        s.selected_board = Some(board.id);
        s.board_request += 1;
        (s.generation, s.board_request, s.account.clone(), s.issue_scope)
    };
    ui.center_title.set_text(&format!("{} · {}", board.name, board.board_type));
    ui.stack.set_visible_child_name("board");
    ui.board_stack.set_visible_child_name("board");
    ui.board_switcher.set_visible(
        board.board_type.eq_ignore_ascii_case("kanban")
            || board.board_type.eq_ignore_ascii_case("scrum")
    );
    ui.search.set_visible(false);
    show_empty(&ui.board_box, "Loading board…", "Fetching issues and columns");
    show_empty(&ui.backlog_box, "Loading backlog…", "Fetching Jira backlog issues");
    show_detail_placeholder(&ui.detail_box);
    let Some(account) = account else { return; };
    set_busy(ui, state, true);
    let sender = sender.clone();
    thread::spawn(move || {
        let result = (|| -> anyhow::Result<BoardData> {
            let client = JiraClient::new(account)?;
            let issues = client.board_issues(board.id, scope.jql())?;
            let columns = client.board_columns(board.id).unwrap_or_default();
            let is_kanban = board.board_type.eq_ignore_ascii_case("kanban");
            let has_backlog = is_kanban || board.board_type.eq_ignore_ascii_case("scrum");
            let backlog = has_backlog
                .then(|| client.board_backlog(board.id, scope.jql()).map_err(|err| err.to_string()));
            Ok(BoardData { issues, columns, backlog, is_kanban })
        })().map_err(|err| err.to_string());
        let _ = sender.send(Event::BoardLoaded(id, board.id, request_id, result));
    });
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

fn render_board_list(ui: &Ui, state: &Rc<RefCell<State>>, sender: &Sender<Event>) {
    clear_box(&ui.boards_box);
    let boards = {
        let s = state.borrow();
        s.pinned_boards.iter()
            .filter_map(|id| s.boards.iter().find(|board| board.id == *id).cloned())
            .collect::<Vec<_>>()
    };
    if boards.is_empty() {
        let message = gtk::Label::new(Some(if state.borrow().boards.is_empty() {
            "No accessible boards"
        } else {
            "Pin boards using ‘Manage boards…’ below."
        }));
        message.set_wrap(true);
        message.add_css_class("dim-label");
        message.set_xalign(0.0);
        ui.boards_box.append(&message);
    }
    for board in boards {
        let button = gtk::Button::new();
        button.add_css_class("flat");
        button.set_halign(gtk::Align::Fill);
        let text = gtk::Label::new(Some(&board.name));
        text.set_xalign(0.0);
        text.set_ellipsize(gtk::pango::EllipsizeMode::End);
        button.set_child(Some(&text));
        ui.boards_box.append(&button);
        let ui = ui.clone();
        let state = state.clone();
        let sender = sender.clone();
        button.connect_clicked(move |_| start_board_load(&ui, &state, &sender, board.clone()));
    }
}

/// The complete board list lives here, never in the sidebar.
fn show_board_picker(ui: &Ui, state: &Rc<RefCell<State>>, sender: &Sender<Event>) {
    let (mut boards, pinned) = {
        let s = state.borrow();
        if s.account.is_none() {
            return;
        }
        (s.boards.clone(), s.pinned_boards.clone())
    };
    // Pinned boards appear first, then the remaining boards alphabetically.
    boards.sort_by(|a, b| {
        let a_pin = pinned.iter().position(|id| *id == a.id);
        let b_pin = pinned.iter().position(|id| *id == b.id);
        match (a_pin, b_pin) {
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        }
    });

    let dialog = gtk::Window::builder()
        .title("Manage boards")
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

    let intro = gtk::Label::new(Some("Select the boards you want in the sidebar."));
    intro.set_xalign(0.0);
    intro.set_wrap(true);
    content.append(&intro);

    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search boards"));
    content.append(&search);

    let rows = gtk::ListBox::new();
    rows.set_selection_mode(gtk::SelectionMode::None);
    rows.add_css_class("boxed-list");
    let mut searchable = Vec::new();

    for board in boards {
        let check = gtk::CheckButton::with_label(&board.name);
        check.set_active(pinned.contains(&board.id));
        check.set_margin_start(12);
        check.set_margin_end(12);
        check.set_margin_top(10);
        check.set_margin_bottom(10);
        check.set_tooltip_text(Some(&format!("{} board · ID {}", board.board_type, board.id)));

        let row = gtk::ListBoxRow::new();
        row.set_activatable(false);
        row.set_child(Some(&check));
        rows.append(&row);
        searchable.push((row, board.name.to_lowercase()));

        let ui = ui.clone();
        let state = state.clone();
        let sender = sender.clone();
        let reverting = Rc::new(Cell::new(false));
        check.connect_toggled(move |check| {
            if reverting.get() {
                return;
            }
            let (account_url, old_pinned, new_pinned) = {
                let mut s = state.borrow_mut();
                let Some(url) = s.account.as_ref().map(|account| account.base_url.clone()) else {
                    return;
                };
                let before = s.pinned_boards.clone();
                if check.is_active() {
                    if !s.pinned_boards.contains(&board.id) {
                        s.pinned_boards.push(board.id);
                    }
                } else {
                    s.pinned_boards.retain(|id| *id != board.id);
                }
                (url, before, s.pinned_boards.clone())
            };
            if let Err(error) = preferences::save_pinned(&account_url, &new_pinned) {
                state.borrow_mut().pinned_boards = old_pinned;
                reverting.set(true);
                check.set_active(!check.is_active());
                reverting.set(false);
                show_error(&ui, &format!("Could not save pinned boards: {error}"));
            } else {
                render_board_list(&ui, &state, &sender);
                // A board may be removed from the sidebar while it is open;
                // leave the current view available until the user navigates.
            }
        });
    }

    {
        let searchable = searchable;
        search.connect_search_changed(move |search| {
            let query = search.text().to_lowercase();
            for (row, name) in &searchable {
                row.set_visible(name.contains(&query));
            }
        });
    }

    let scroller = gtk::ScrolledWindow::new();
    scroller.set_vexpand(true);
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_child(Some(&rows));
    content.append(&scroller);

    if rows.first_child().is_none() {
        let empty = gtk::Label::new(Some("No accessible Jira boards."));
        empty.add_css_class("dim-label");
        content.append(&empty);
    }

    let done = gtk::Button::with_label("Done");
    done.set_halign(gtk::Align::End);
    done.add_css_class("suggested-action");
    content.append(&done);
    dialog.set_child(Some(&content));
    {
        let dialog = dialog.clone();
        done.connect_clicked(move |_| dialog.close());
    }
    dialog.present();
    search.grab_focus();
}

fn render_my_issues(ui: &Ui, state: &State) {
    clear_box(&ui.issue_box);
    let issues = &state.issues;
    let filter = ui.search.text().to_lowercase();
    let filtered: Vec<&Issue> = issues.iter().filter(|issue| {
        filter.is_empty()
            || issue.key.to_lowercase().contains(&filter)
            || issue.summary.to_lowercase().contains(&filter)
            || issue.status.to_lowercase().contains(&filter)
    }).collect();
    if filtered.is_empty() {
        show_empty(&ui.issue_box, "No matching issues", "Try a different search, or refresh.");
        return;
    }
    let base_url = state.account.as_ref().map(|a| a.base_url.as_str()).unwrap_or("");
    for issue in filtered {
        let button = issue_button(issue, &ui.detail_box, base_url);
        ui.issue_box.append(&button);
    }
}

fn render_board(ui: &Ui, issues: Vec<Issue>, columns: Vec<BoardColumn>, base_url: &str, is_kanban: bool) {
    clear_box(&ui.board_box);
    // Kanban's separately configured Backlog is not an active board column.
    let columns: Vec<BoardColumn> = columns.into_iter()
        .filter(|column| !(is_kanban && column.name.eq_ignore_ascii_case("backlog")))
        .collect();
    let mut groups: Vec<(String, Vec<Issue>)> = columns.iter()
        .map(|c| (c.name.clone(), Vec::new())).collect();
    if groups.is_empty() {
        for issue in &issues {
            if !groups.iter().any(|(name, _)| name == &issue.status) {
                groups.push((issue.status.clone(), Vec::new()));
            }
        }
    }
    for issue in issues {
        let idx = if columns.is_empty() {
            groups.iter().position(|(name, _)| name == &issue.status)
        } else {
            columns.iter().position(|c| c.status_ids.contains(&issue.status_id))
        };
        if let Some(idx) = idx {
            groups[idx].1.push(issue);
        } else {
            if groups.last().map(|(name, _)| name.as_str()) != Some("Other") {
                groups.push(("Other".into(), Vec::new()));
            }
            groups.last_mut().unwrap().1.push(issue);
        }
    }
    if groups.is_empty() {
        show_empty(&ui.board_box, "Board is empty", "No issues are mapped to board columns.");
        return;
    }
    for (name, items) in groups {
        let column = gtk::Box::new(gtk::Orientation::Vertical, 8);
        column.set_width_request(275);
        let title = gtk::Label::new(Some(&format!("{name}  ({})", items.len())));
        title.add_css_class("heading");
        title.set_xalign(0.0);
        column.append(&title);
        let item_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        for issue in &items {
            item_box.append(&issue_button(issue, &ui.detail_box, base_url));
        }
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_child(Some(&item_box));
        scroll.set_vexpand(true);
        column.append(&scroll);
        ui.board_box.append(&column);
    }
}

fn render_backlog(ui: &Ui, issues: &[Issue], base_url: &str) {
    clear_box(&ui.backlog_box);
    if issues.is_empty() {
        show_empty(&ui.backlog_box, "Backlog is empty", "There are no issues in this backlog.");
        return;
    }

    let count = gtk::Label::new(Some(&format!("{} issues · ordered by Jira rank", issues.len())));
    count.add_css_class("dim-label");
    count.set_xalign(0.0);
    count.set_margin_bottom(8);
    ui.backlog_box.append(&count);

    for issue in issues {
        let row = gtk::Button::new();
        row.add_css_class("flat");
        row.set_halign(gtk::Align::Fill);
        let contents = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        contents.set_margin_top(7);
        contents.set_margin_bottom(7);
        contents.set_margin_start(8);
        contents.set_margin_end(8);

        let key = gtk::Label::new(Some(&issue.key));
        key.add_css_class("dim-label");
        key.set_width_chars(13);
        key.set_xalign(0.0);
        contents.append(&key);

        let summary = gtk::Label::new(Some(&issue.summary));
        summary.set_xalign(0.0);
        summary.set_hexpand(true);
        summary.set_ellipsize(gtk::pango::EllipsizeMode::End);
        contents.append(&summary);

        let status = gtk::Label::new(Some(&issue.status));
        status.add_css_class("dim-label");
        status.set_ellipsize(gtk::pango::EllipsizeMode::End);
        contents.append(&status);

        row.set_child(Some(&contents));
        let issue = issue.clone();
        let detail_box = ui.detail_box.clone();
        let base_url = base_url.to_owned();
        row.connect_clicked(move |_| show_detail(&detail_box, &issue, &base_url));
        ui.backlog_box.append(&row);
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
    fn board_scope_uses_server_side_jql() {
        assert_eq!(IssueScope::Mine.jql(), Some("assignee = currentUser()"));
        assert_eq!(IssueScope::Unassigned.jql(), Some("assignee IS EMPTY"));
        assert_eq!(IssueScope::All.jql(), None);
    }
}
