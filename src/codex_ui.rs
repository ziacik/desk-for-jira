//! Native Codex conversation window, deliberately separate from VS Code and terminals.
use std::{cell::Cell, path::Path, rc::Rc, sync::mpsc::{self, TryRecvError}, thread, time::Duration};

use adw::prelude::*;
use gtk::glib;

use crate::{
    codex, codex_session::{self, Control, Event}, dialog, git,
    jira::{Credentials, Issue, JiraClient},
};

fn append(log: &gtk::TextView, value: &str) {
    let buffer = log.buffer();
    let mut end = buffer.end_iter();
    buffer.insert(&mut end, value);
    log.scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0);
}

fn submit(input: &gtk::TextView, messages: &gtk::TextView, send: &gtk::Button,
          stop: &gtk::Button, commands: &mpsc::Sender<Control>) {
    let buffer = input.buffer();
    let message = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string();
    if message.trim().is_empty() { return; }
    if commands.send(Control::Send(message.clone())).is_err() { return; }
    append(messages, &format!("\nYou: {}\n\n", message.trim()));
    buffer.set_text("");
    send.set_sensitive(false);
    stop.set_sensitive(true);
}

pub fn open(
    parent: &impl IsA<gtk::Window>, worktree: &Path, site: &str,
    issue: &Issue, account: Option<Credentials>,
) {
    let window = gtk::Window::builder()
        .title(format!("Codex · {}", issue.key))
        .transient_for(parent)
        .default_width(820).default_height(620)
        .build();
    dialog::close_on_escape(&window);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 10);
    body.set_margin_top(15);
    body.set_margin_bottom(15);
    body.set_margin_start(15);
    body.set_margin_end(15);

    let heading = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let title = gtk::Label::new(Some(&format!("Codex · {}", issue.key)));
    title.add_css_class("title-3");
    title.set_xalign(0.0);
    title.set_hexpand(true);
    heading.append(&title);
    let external = gtk::Button::with_label("Open in Terminal");
    external.add_css_class("flat");
    external.set_tooltip_text(Some("Explicitly launch a separate Codex CLI session"));
    heading.append(&external);
    body.append(&heading);

    let worktree_label = gtk::Label::new(Some(&worktree.to_string_lossy()));
    worktree_label.add_css_class("dim-label");
    worktree_label.set_xalign(0.0);
    worktree_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    worktree_label.set_selectable(true);
    worktree_label.set_tooltip_text(Some(&worktree.to_string_lossy()));
    body.append(&worktree_label);

    let scroller = gtk::ScrolledWindow::new();
    scroller.set_vexpand(true);
    scroller.set_hexpand(true);
    scroller.add_css_class("frame");
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    let messages = gtk::TextView::new();
    messages.set_editable(false);
    messages.set_cursor_visible(false);
    messages.set_wrap_mode(gtk::WrapMode::WordChar);
    messages.set_top_margin(12);
    messages.set_bottom_margin(12);
    messages.set_left_margin(12);
    messages.set_right_margin(12);
    scroller.set_child(Some(&messages));
    body.append(&scroller);

    let approvals = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.append(&approvals);

    let status = gtk::Label::new(Some("Starting Codex…"));
    status.add_css_class("dim-label");
    status.set_xalign(0.0);
    status.set_wrap(true);
    body.append(&status);

    let composer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let input_frame = gtk::ScrolledWindow::new();
    input_frame.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    input_frame.set_min_content_height(72);
    input_frame.set_max_content_height(120);
    input_frame.set_hexpand(true);
    input_frame.add_css_class("frame");
    let input = gtk::TextView::new();
    input.set_wrap_mode(gtk::WrapMode::WordChar);
    input.set_top_margin(8);
    input.set_left_margin(8);
    input.set_right_margin(8);
    input.set_bottom_margin(8);
    input_frame.set_child(Some(&input));
    composer.append(&input_frame);
    let send = gtk::Button::with_label("Send");
    send.add_css_class("suggested-action");
    send.set_sensitive(false);
    composer.append(&send);
    let stop = gtk::Button::from_icon_name("media-playback-stop-symbolic");
    stop.set_tooltip_text(Some("Interrupt the current Codex turn"));
    stop.set_sensitive(false);
    composer.append(&stop);
    body.append(&composer);
    window.set_child(Some(&body));

    let (commands, events) = codex_session::spawn(
        worktree.to_path_buf(), site.to_owned(), issue.clone(), account.clone(),
    );

    {
        let input = input.clone();
        let messages = messages.clone();
        let send_button = send.clone();
        let stop_button = stop.clone();
        let commands = commands.clone();
        send.connect_clicked(move |_| {
            submit(&input, &messages, &send_button, &stop_button, &commands);
        });
    }
    {
        let keys = gtk::EventControllerKey::new();
        let input = input.clone();
        let messages = messages.clone();
        let send = send.clone();
        let stop = stop.clone();
        let commands = commands.clone();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            if key == gtk::gdk::Key::Return
                && modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                && send.is_sensitive()
            {
                submit(&input, &messages, &send, &stop, &commands);
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        input.add_controller(keys);
    }
    {
        let commands = commands.clone();
        stop.connect_clicked(move |_| {
            let _ = commands.send(Control::Interrupt);
        });
    }
    {
        let commands = commands.clone();
        window.connect_close_request(move |_| {
            let _ = commands.send(Control::Quit);
            glib::Propagation::Proceed
        });
    }
    {
        let worktree = worktree.to_path_buf();
        let site = site.to_owned();
        let issue = issue.clone();
        let status = status.clone();
        external.connect_clicked(move |button| {
            button.set_sensitive(false);
            let (sender, receiver) = mpsc::channel();
            let path = worktree.clone();
            let site = site.clone();
            let issue = issue.clone();
            let account = account.clone();
            thread::spawn(move || {
                let comments = account
                    .and_then(|credentials| JiraClient::new(credentials).ok())
                    .and_then(|client| client.issue_comments(&issue.key).ok())
                    .unwrap_or_default();
                let result = codex::start(&path, &site, &issue, &comments)
                    .map_err(|error| error.to_string());
                let _ = sender.send(result);
            });
            let button = button.clone();
            let status = status.clone();
            glib::timeout_add_local(Duration::from_millis(90), move || {
                match receiver.try_recv() {
                    Ok(result) => {
                        button.set_sensitive(true);
                        status.set_text(&match result {
                            Ok(()) => "External Codex launched".to_owned(),
                            Err(error) => format!("Cannot launch Codex: {error}"),
                        });
                        glib::ControlFlow::Break
                    }
                    Err(TryRecvError::Empty) => glib::ControlFlow::Continue,
                    Err(TryRecvError::Disconnected) => glib::ControlFlow::Break,
                }
            });
        });
    }

    let connected = Rc::new(Cell::new(false));
    let busy = Rc::new(Cell::new(false));
    glib::timeout_add_local(Duration::from_millis(55), move || {
        for _ in 0..100 {
            match events.try_recv() {
                Ok(Event::Ready) => {
                    connected.set(true);
                    send.set_sensitive(!busy.get());
                    status.set_text("Ready");
                }
                Ok(Event::Busy(value)) => {
                    busy.set(value);
                    send.set_sensitive(connected.get() && !value);
                    stop.set_sensitive(value);
                    if value { status.set_text("Codex is working…"); }
                }
                Ok(Event::Output(value)) => append(&messages, &value),
                Ok(Event::Status(value)) => status.set_text(&value),
                Ok(Event::Error(value)) => {
                    status.set_text(&value);
                    append(&messages, &format!("\nCodex error: {value}\n"));
                }
                Ok(Event::Approval {request_id, description}) => {
                    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                    row.add_css_class("card");
                    let label = gtk::Label::new(Some(&description));
                    label.set_wrap(true);
                    label.set_xalign(0.0);
                    label.set_hexpand(true);
                    row.append(&label);
                    let allow = gtk::Button::with_label("Allow");
                    allow.add_css_class("suggested-action");
                    row.append(&allow);
                    let deny = gtk::Button::with_label("Deny");
                    row.append(&deny);
                    {
                        let commands = commands.clone();
                        let approvals = approvals.clone();
                        let row = row.clone();
                        let request_id = request_id.clone();
                        allow.connect_clicked(move |_| {
                            let _ = commands.send(Control::Decide {request_id: request_id.clone(), allow: true});
                            approvals.remove(&row);
                        });
                    }
                    {
                        let commands = commands.clone();
                        let approvals = approvals.clone();
                        let row = row.clone();
                        deny.connect_clicked(move |_| {
                            let _ = commands.send(Control::Decide {request_id: request_id.clone(), allow: false});
                            approvals.remove(&row);
                        });
                    }
                    approvals.append(&row);
                }
                Ok(Event::Closed) => {
                    send.set_sensitive(false);
                    stop.set_sensitive(false);
                    return glib::ControlFlow::Break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return glib::ControlFlow::Break,
            }
        }
        glib::ControlFlow::Continue
    });
    window.present();
}
