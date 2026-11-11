//! Pull requests connected to a Jira issue through the Development panel.
//! Fetches on a worker thread so opening a ticket never blocks GTK.
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::mpsc::{self, TryRecvError},
    thread,
    time::Duration,
};

use adw::prelude::*;
use gtk::glib;

use crate::jira::{Credentials, JiraClient, PullRequest};

#[derive(Clone)]
pub struct PullRequestsPanel {
    root: gtk::Box,
    list: gtk::Box,
    count: gtk::Label,
    request: Rc<Cell<u64>>,
    context: Rc<RefCell<Option<(Credentials, String)>>>,
}

impl PullRequestsPanel {
    pub fn new() -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
        root.add_css_class("ticket-pr-section");
        root.set_visible(false);

        let heading = gtk::Box::new(gtk::Orientation::Horizontal, 9);
        let icon = gtk::Image::from_icon_name("insert-link-symbolic");
        icon.add_css_class("ticket-pr-icon");
        heading.append(&icon);
        let title = gtk::Label::new(Some("Pull requests"));
        title.add_css_class("ticket-pr-title");
        title.set_xalign(0.0);
        title.set_hexpand(true);
        heading.append(&title);
        let count = gtk::Label::new(None);
        count.add_css_class("ticket-pr-count");
        heading.append(&count);
        let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh.add_css_class("flat");
        refresh.add_css_class("ticket-pr-refresh");
        refresh.set_tooltip_text(Some("Refresh linked pull requests"));
        heading.append(&refresh);
        root.append(&heading);

        let list = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.append(&list);

        let panel = Self {
            root, list, count,
            request: Rc::new(Cell::new(0)),
            context: Rc::new(RefCell::new(None)),
        };
        let panel_for_refresh = panel.clone();
        refresh.connect_clicked(move |_| {
            let context = panel_for_refresh.context.borrow().clone();
            if let Some((account, key)) = context {
                panel_for_refresh.refresh(Some(account), &key);
            }
        });
        panel
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn refresh(&self, account: Option<Credentials>, key: &str) {
        let request = self.request.get().wrapping_add(1);
        self.request.set(request);
        *self.context.borrow_mut() = account.as_ref()
            .map(|account| (account.clone(), key.to_owned()));
        self.clear();
        let Some(account) = account else {
            self.root.set_visible(false);
            return;
        };
        self.root.set_visible(true);
        self.count.set_text("Checking…");
        self.message("Looking up linked PRs…", None);

        let key = key.to_owned();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = JiraClient::new(account)
                .and_then(|client| client.issue_pull_requests(&key))
                .map_err(|error| error.to_string());
            let _ = sender.send(result);
        });

        let panel = self.clone();
        glib::timeout_add_local(Duration::from_millis(100), move || {
            match receiver.try_recv() {
                Ok(result) => {
                    if panel.request.get() == request {
                        panel.paint(result);
                    }
                    glib::ControlFlow::Break
                }
                Err(TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(TryRecvError::Disconnected) => {
                    if panel.request.get() == request {
                        panel.paint(Err("PR lookup stopped unexpectedly".into()));
                    }
                    glib::ControlFlow::Break
                }
            }
        });
    }

    fn clear(&self) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
    }

    fn message(&self, text: &str, details: Option<&str>) {
        let label = gtk::Label::new(Some(text));
        label.set_xalign(0.0);
        label.set_wrap(true);
        label.add_css_class("ticket-pr-muted");
        if let Some(details) = details {
            label.set_tooltip_text(Some(details));
        }
        self.list.append(&label);
    }

    fn paint(&self, result: Result<Vec<PullRequest>, String>) {
        self.clear();
        match result {
            Ok(requests) if requests.is_empty() => {
                self.root.set_visible(false);
            }
            Ok(requests) => {
                self.root.set_visible(true);
                self.count.set_text(&requests.len().to_string());
                for request in requests {
                    self.append_request(&request);
                }
            }
            Err(error) => {
                self.root.set_visible(true);
                self.count.set_text("Unavailable");
                self.message("Could not load linked PRs from Jira.", Some(&error));
            }
        }
    }

    fn append_request(&self, request: &PullRequest) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.add_css_class("ticket-pr-row");

        let link = gtk::LinkButton::with_label(&request.url, &request.title);
        link.set_hexpand(true);
        link.set_halign(gtk::Align::Fill);
        link.set_tooltip_text(Some(&request.url));
        link.add_css_class("ticket-pr-link");
        let label = gtk::Label::new(Some(&request.title));
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_hexpand(true);
        link.set_child(Some(&label));
        row.append(&link);

        let (name, class) = match request.status.as_str() {
            "OPEN" | "ACTIVE" => ("Open", "ticket-pr-open"),
            "MERGED" | "COMPLETED" => ("Merged", "ticket-pr-merged"),
            "DECLINED" | "ABANDONED" | "CLOSED" => ("Closed", "ticket-pr-closed"),
            "" => ("", "ticket-pr-unknown"),
            state => (state, "ticket-pr-unknown"),
        };
        if !name.is_empty() {
            let status = gtk::Label::new(Some(name));
            status.add_css_class("ticket-pr-status");
            status.add_css_class(class);
            row.append(&status);
        }
        self.list.append(&row);
    }
}
