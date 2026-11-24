//! Native Codex conversation workspace, hosted in the main window's tabs.
use std::{cell::{Cell, RefCell}, path::{Path, PathBuf}, rc::Rc, sync::mpsc::{self, TryRecvError}, thread, time::Duration};

use adw::prelude::*;
use gtk::{gdk, glib};
use gtk::glib::prelude::StaticType;
use base64::{Engine as _, engine::general_purpose::STANDARD};

use crate::{
    codex, codex_markdown, codex_session::{self, Control, Event, TranscriptItem},
    jira::{Credentials, Issue, JiraClient},
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionKey {
    site: String,
    issue_key: String,
    worktree: PathBuf,
}

struct CodexTab {
    key: SessionKey,
    page: adw::TabPage,
    commands: mpsc::Sender<Control>,
}

thread_local! {
    static TAB_VIEW: RefCell<Option<adw::TabView>> = const { RefCell::new(None) };
    static CODEX_TABS: RefCell<Vec<CodexTab>> = const { RefCell::new(Vec::new()) };
}

/// Install the main application's tab view once; all Codex launch points use it.
pub fn install_tabs(tabs: &adw::TabView) {
    TAB_VIEW.with(|slot| *slot.borrow_mut() = Some(tabs.clone()));
}

/// Called by the main tab view when a Codex page is closed.
pub fn forget_page(page: &adw::TabPage) {
    CODEX_TABS.with(|slot| {
        let mut sessions = slot.borrow_mut();
        if let Some(index) = sessions.iter().position(|session| &session.page == page) {
            let session = sessions.remove(index);
            let _ = session.commands.send(Control::Quit);
        }
    });
}

/// Close transports cleanly when Desk for Jira is exiting.
pub fn shutdown() {
    CODEX_TABS.with(|slot| {
        for session in slot.borrow().iter() {
            let _ = session.commands.send(Control::Quit);
        }
    });
}

fn clear(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn readable_label(text: &str, class: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_selectable(true);
    label.add_css_class(class);
    label
}

pub(crate) fn show_markdown(container: &gtk::Box, source: &str) {
    clear(container);
    for block in codex_markdown::blocks(source) {
        let kind = match &block {
            codex_markdown::Block::Heading(_) => "codex-chat-heading",
            codex_markdown::Block::Quote(_) => "codex-chat-quote",
            _ => "codex-chat-paragraph",
        };
        match block {
            codex_markdown::Block::Paragraph(markup)
            | codex_markdown::Block::Heading(markup)
            | codex_markdown::Block::Quote(markup) => {
                let label = readable_label("", kind);
                label.set_markup(&markup);
                container.append(&label);
            }
            codex_markdown::Block::Code(code) => {
                let label = gtk::Label::new(Some(code.trim_end_matches('\n')));
                label.set_xalign(0.0);
                label.set_selectable(true);
                label.add_css_class("codex-chat-code");
                let scroll = gtk::ScrolledWindow::new();
                scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Never);
                scroll.set_min_content_width(1);
                scroll.add_css_class("codex-chat-code-scroll");
                scroll.set_child(Some(&label));
                container.append(&scroll);
            }
            codex_markdown::Block::Rule => {
                container.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            }
        }
    }
}

struct AgentMessage {
    text: String,
    body: gtk::Box,
    dirty: bool,
}

struct Transcript {
    root: gtk::Box,
    scroll: gtk::ScrolledWindow,
    agent: Option<AgentMessage>,
    command_output: Option<gtk::TextBuffer>,
}

impl Transcript {
    fn new(scroll: &gtk::ScrolledWindow) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.add_css_class("codex-chat-transcript");
        root.set_margin_top(12);
        root.set_margin_bottom(12);
        root.set_margin_start(12);
        root.set_margin_end(12);
        scroll.set_child(Some(&root));
        Self { root, scroll: scroll.clone(), agent: None, command_output: None }
    }

    fn should_follow(&self) -> bool {
        let adjustment = self.scroll.vadjustment();
        adjustment.value() + adjustment.page_size() + 72.0 >= adjustment.upper()
    }

    fn follow(&self, enabled: bool) {
        if enabled {
            let adjustment = self.scroll.vadjustment();
            glib::idle_add_local_once(move || {
                adjustment.set_value((adjustment.upper() - adjustment.page_size())
                    .max(adjustment.lower()));
            });
        }
    }

    fn message(&mut self, role: &str, user: bool) -> gtk::Box {
        self.finish_agent();
        self.command_output = None;
        let item = gtk::Box::new(gtk::Orientation::Vertical, 7);
        item.add_css_class("codex-chat-message");
        if user { item.add_css_class("codex-chat-user"); }
        let name = readable_label(role, "codex-chat-role");
        name.set_selectable(false);
        item.append(&name);
        let body = gtk::Box::new(gtk::Orientation::Vertical, 7);
        body.add_css_class("codex-chat-body");
        item.append(&body);
        self.root.append(&item);
        body
    }

    fn user(&mut self, text: &str) {
        let follow = self.should_follow();
        let body = self.message("You", true);
        show_markdown(&body, text);
        self.follow(follow);
    }

    fn agent_delta(&mut self, text: &str) {
        if self.agent.is_none() {
            let body = self.message("Codex", false);
            self.agent = Some(AgentMessage { text: String::new(), body, dirty: false });
        }
        if let Some(agent) = self.agent.as_mut() {
            agent.text.push_str(text);
            agent.dirty = true;
        }
    }

    fn flush_agent(&mut self) {
        if self.agent.as_ref().is_some_and(|agent| agent.dirty) {
            let follow = self.should_follow();
            if let Some(agent) = self.agent.as_mut() {
                show_markdown(&agent.body, &agent.text);
                agent.dirty = false;
            }
            self.follow(follow);
        }
    }

    fn finish_agent(&mut self) {
        self.flush_agent();
        self.agent = None;
    }

    fn command(&mut self, command: &str) {
        let follow = self.should_follow();
        self.finish_agent();
        let summary_line = command.lines().next().unwrap_or(command);
        let summary: String = summary_line.chars().take(95).collect();
        let summary = if summary_line.chars().count() > 95 {
            format!("{summary}…")
        } else { summary };
        let expander = gtk::Expander::new(Some(&format!("$ {summary}")));
        expander.add_css_class("codex-chat-command");
        let scroller = gtk::ScrolledWindow::new();
        scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
        scroller.set_min_content_height(70);
        scroller.set_max_content_height(260);
        scroller.set_propagate_natural_height(true);
        let output = gtk::TextView::new();
        output.set_editable(false);
        output.set_cursor_visible(false);
        output.set_monospace(true);
        output.set_wrap_mode(gtk::WrapMode::WordChar);
        output.set_top_margin(9);
        output.set_bottom_margin(9);
        output.set_left_margin(10);
        output.set_right_margin(10);
        let buffer = output.buffer();
        buffer.set_text(&format!("$ {command}\n"));
        scroller.set_child(Some(&output));
        expander.set_child(Some(&scroller));
        self.root.append(&expander);
        self.command_output = Some(buffer);
        self.follow(follow);
    }

    fn command_delta(&mut self, chunk: &str) {
        if let Some(buffer) = self.command_output.as_ref() {
            let mut end = buffer.end_iter();
            buffer.insert(&mut end, chunk);
        }
    }

    fn info(&mut self, text: &str) {
        let follow = self.should_follow();
        self.finish_agent();
        self.command_output = None;
        let label = readable_label(text, "codex-chat-info");
        self.root.append(&label);
        self.follow(follow);
    }

    fn output(&mut self, value: TranscriptItem) {
        match value {
            TranscriptItem::User(text) => self.user(&text),
            TranscriptItem::AgentDelta(text) => self.agent_delta(&text),
            TranscriptItem::AgentEnd => self.finish_agent(),
            TranscriptItem::Command(text) => self.command(&text),
            TranscriptItem::CommandOutput(text) => self.command_delta(&text),
            TranscriptItem::Info(text) => self.info(&text),
        }
    }
}


#[derive(Clone)]
struct PendingImage {
    input: codex_session::ImageInput,
    preview: gdk::Texture,
    name: String,
}

/// Keep images separate from the text buffer so screenshots are genuine
/// multimodal Codex inputs rather than image paths pasted into the prompt.
fn refresh_image_previews(
    images: &Rc<RefCell<Vec<PendingImage>>>, strip: &gtk::ScrolledWindow, previews: &gtk::Box,
) {
    clear(previews);
    strip.set_visible(!images.borrow().is_empty());
    for (index, image) in images.borrow().iter().enumerate() {
        let tile = gtk::Box::new(gtk::Orientation::Vertical, 3);
        let overlay = gtk::Overlay::new();
        let picture = gtk::Picture::for_paintable(&image.preview);
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.set_size_request(92, 72);
        overlay.set_child(Some(&picture));

        let remove = gtk::Button::from_icon_name("window-close-symbolic");
        remove.add_css_class("flat");
        remove.add_css_class("circular");
        remove.set_halign(gtk::Align::End);
        remove.set_valign(gtk::Align::Start);
        remove.set_tooltip_text(Some("Remove image"));
        overlay.add_overlay(&remove);
        tile.append(&overlay);

        let label = gtk::Label::new(Some(&image.name));
        label.set_max_width_chars(14);
        label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        label.set_tooltip_text(Some(&image.name));
        tile.append(&label);
        previews.append(&tile);

        // The remove button lives below 'previews': only capture weak widget
        // refs to avoid a widget/closure reference cycle.
        let previews_weak = previews.downgrade();
        let strip_weak = strip.downgrade();
        let images = images.clone();
        remove.connect_clicked(move |_| {
            images.borrow_mut().remove(index);
            if let (Some(strip), Some(previews)) =
                (strip_weak.upgrade(), previews_weak.upgrade())
            {
                refresh_image_previews(&images, &strip, &previews);
            }
        });
    }
}

fn append_image(
    images: &Rc<RefCell<Vec<PendingImage>>>, strip: &gtk::ScrolledWindow,
    previews: &gtk::Box, input: codex_session::ImageInput,
    preview: gdk::Texture, name: String,
) {
    images.borrow_mut().push(PendingImage { input, preview, name });
    refresh_image_previews(images, strip, previews);
}

fn submit(
    input: &gtk::TextView, images: &Rc<RefCell<Vec<PendingImage>>>,
    strip: &gtk::ScrolledWindow, previews: &gtk::Box,
    transcript: &Rc<RefCell<Transcript>>, send: &gtk::Button,
    stop: &gtk::Button, commands: &mpsc::Sender<Control>,
) {
    let buffer = input.buffer();
    let message = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string();
    let attachments: Vec<_> = images.borrow().iter()
        .map(|image| image.input.clone()).collect();
    let count = attachments.len();
    if message.trim().is_empty() && count == 0 { return; }
    if commands.send(Control::Send { text: message.clone(), images: attachments }).is_err() {
        return;
    }
    let mut display = message.trim().to_owned();
    if count != 0 {
        if !display.is_empty() { display.push('\n'); }
        display.push_str(&format!("[{count} image{} attached]", if count == 1 { "" } else { "s" }));
    }
    transcript.borrow_mut().user(&display);
    buffer.set_text("");
    images.borrow_mut().clear();
    refresh_image_previews(images, strip, previews);
    send.set_sensitive(false);
    stop.set_sensitive(true);
}

pub fn open(
    _parent: &impl IsA<gtk::Window>, worktree: &Path, site: &str,
    issue: &Issue, account: Option<Credentials>,
) -> bool {
    let Some(tabs) = TAB_VIEW.with(|slot| slot.borrow().clone()) else {
        eprintln!("Cannot open Codex: main tab view is not installed");
        return false;
    };
    let key = SessionKey {
        site: site.to_owned(), issue_key: issue.key.clone(),
        worktree: worktree.to_path_buf(),
    };
    let existing = CODEX_TABS.with(|slot| slot.borrow().iter()
        .find(|tab| tab.key == key).map(|tab| tab.page.clone()));
    if let Some(page) = existing {
        tabs.set_selected_page(&page);
        return true;
    }
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
    let transcript = Rc::new(RefCell::new(Transcript::new(&scroller)));
    body.append(&scroller);

    let approvals = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.append(&approvals);

    let status = gtk::Label::new(Some("Starting Codex…"));
    status.add_css_class("dim-label");
    status.set_xalign(0.0);
    status.set_wrap(true);
    body.append(&status);

    let images: Rc<RefCell<Vec<PendingImage>>> = Rc::new(RefCell::new(Vec::new()));
    let attachment_strip = gtk::ScrolledWindow::new();
    attachment_strip.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Never);
    attachment_strip.set_min_content_height(112);
    attachment_strip.set_visible(false);
    let attachment_previews = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    attachment_previews.set_margin_top(5);
    attachment_previews.set_margin_bottom(5);
    attachment_strip.set_child(Some(&attachment_previews));
    body.append(&attachment_strip);

    let composer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let attach = gtk::Button::from_icon_name("mail-attachment-symbolic");
    attach.set_tooltip_text(Some("Attach image files"));
    composer.append(&attach);
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
    let page = tabs.append(&body);
    page.set_title(&format!("Codex · {}", issue.key));
    page.set_loading(true);
    tabs.set_selected_page(&page);

    let (commands, events) = codex_session::spawn(
        worktree.to_path_buf(), site.to_owned(), issue.clone(), account.clone(),
    );

    // A native file chooser is an alternative to pasting screenshots. Codex
    // receives real local image paths, not just their names in the prompt.
    {
        let parent = _parent.as_ref().clone();
        let images = images.clone();
        let strip = attachment_strip.clone();
        let previews = attachment_previews.clone();
        let status = status.clone();
        attach.connect_clicked(move |_| {
            let picker = gtk::FileDialog::new();
            picker.set_title("Attach images");
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("Images (PNG, JPEG, TIFF)"));
            for mime in ["image/png", "image/jpeg", "image/tiff"] {
                filter.add_mime_type(mime);
            }
            let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
            filters.append(&filter);
            picker.set_filters(Some(&filters));

            let parent = parent.clone();
            let images = images.clone();
            let strip = strip.clone();
            let previews = previews.clone();
            let status = status.clone();
            glib::MainContext::default().spawn_local(async move {
                if let Ok(files) = picker.open_multiple_future(Some(&parent)).await {
                    for file in files.iter::<gtk::gio::File>().flatten() {
                        let Some(path) = file.path() else { continue };
                        match gdk::Texture::from_filename(&path) {
                            Ok(preview) => {
                                let name = path.file_name()
                                    .map(|name| name.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| "Image".to_owned());
                                append_image(&images, &strip, &previews,
                                    codex_session::ImageInput::Local(path), preview, name);
                            }
                            Err(error) => status.set_text(&format!("Cannot attach image: {error}")),
                        }
                    }
                }
            });
        });
    }
    {
        let input = input.clone();
        let images = images.clone();
        let strip = attachment_strip.clone();
        let previews = attachment_previews.clone();
        let transcript = transcript.clone();
        let send_button = send.clone();
        let stop_button = stop.clone();
        let commands = commands.clone();
        send.connect_clicked(move |_| {
            submit(&input, &images, &strip, &previews,
                &transcript, &send_button, &stop_button, &commands);
        });
    }
    {
        let keys = gtk::EventControllerKey::new();
        let input = input.clone();
        let controller_target = input.clone();
        let images = images.clone();
        let strip = attachment_strip.clone();
        let previews = attachment_previews.clone();
        let status = status.clone();
        let transcript = transcript.clone();
        let send = send.clone();
        let stop = stop.clone();
        let commands = commands.clone();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            if key == gdk::Key::Return
                && modifiers.contains(gdk::ModifierType::CONTROL_MASK)
                && send.is_sensitive()
            {
                submit(&input, &images, &strip, &previews,
                    &transcript, &send, &stop, &commands);
                return glib::Propagation::Stop;
            }
            if matches!(key, gdk::Key::v | gdk::Key::V)
                && modifiers.contains(gdk::ModifierType::CONTROL_MASK)
            {
                let clipboard = input.display().clipboard();
                let formats = clipboard.formats();
                let is_image = formats.contains_type(gdk::Texture::static_type())
                    || ["image/png", "image/jpeg", "image/tiff", "image/webp", "image/bmp"]
                        .iter().any(|mime| formats.contain_mime_type(mime));
                if is_image {
                    let images = images.clone();
                    let strip = strip.clone();
                    let previews = previews.clone();
                    let status = status.clone();
                    glib::MainContext::default().spawn_local(async move {
                        match clipboard.read_texture_future().await {
                            Ok(Some(texture)) => {
                                let png = texture.save_to_png_bytes();
                                let url = format!("data:image/png;base64,{}", STANDARD.encode(png.as_ref()));
                                append_image(&images, &strip, &previews,
                                    codex_session::ImageInput::InlinePng(url),
                                    texture, "Pasted image".to_owned());
                            }
                            Ok(None) => status.set_text("No image in clipboard"),
                            Err(error) => status.set_text(&format!("Cannot paste image: {error}")),
                        }
                    });
                    return glib::Propagation::Stop;
                }
            }
            glib::Propagation::Proceed
        });
        controller_target.add_controller(keys);
    }
    {
        let commands = commands.clone();
        stop.connect_clicked(move |_| {
            let _ = commands.send(Control::Interrupt);
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

    CODEX_TABS.with(|slot| slot.borrow_mut().push(CodexTab {
        key, page: page.clone(), commands: commands.clone(),
    }));
    let connected = Rc::new(Cell::new(false));
    let busy = Rc::new(Cell::new(false));
    glib::timeout_add_local(Duration::from_millis(55), move || {
        for _ in 0..100 {
            match events.try_recv() {
                Ok(Event::Ready) => {
                    page.set_loading(false);
                    connected.set(true);
                    send.set_sensitive(!busy.get());
                    status.set_text("Ready");
                }
                Ok(Event::Busy(value)) => {
                    page.set_loading(value);
                    let finished = busy.replace(value) && !value;
                    if finished && tabs.selected_page().as_ref() != Some(&page) {
                        page.set_needs_attention(true);
                    }
                    send.set_sensitive(connected.get() && !value);
                    stop.set_sensitive(value);
                    if value { status.set_text("Codex is working…"); }
                }
                Ok(Event::Output(value)) => transcript.borrow_mut().output(value),
                Ok(Event::Status(value)) => status.set_text(&value),
                Ok(Event::Error(value)) => {
                    page.set_loading(false);
                    if tabs.selected_page().as_ref() != Some(&page) {
                        page.set_needs_attention(true);
                    }
                    status.set_text(&value);
                    transcript.borrow_mut().info(&format!("Codex error: {value}"));
                }
                Ok(Event::Approval {request_id, description}) => {
                    if tabs.selected_page().as_ref() != Some(&page) {
                        page.set_needs_attention(true);
                    }
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
                    page.set_loading(false);
                    send.set_sensitive(false);
                    stop.set_sensitive(false);
                    return glib::ControlFlow::Break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    page.set_loading(false);
                    return glib::ControlFlow::Break;
                },
            }
        }
        transcript.borrow_mut().flush_agent();
        glib::ControlFlow::Continue
    });
    true
}

#[cfg(test)]
mod tab_tests {
    use super::SessionKey;
    use std::path::PathBuf;

    #[test]
    fn sessions_are_unique_per_site_issue_and_worktree() {
        let key = SessionKey {
            site: "https://jira.example".into(),
            issue_key: "ART-123".into(),
            worktree: PathBuf::from("/worktrees/repo/ART-123"),
        };
        assert_eq!(key, key.clone());
        assert_ne!(key, SessionKey {issue_key: "ART-456".into(), ..key.clone()});
        assert_ne!(key, SessionKey {worktree: PathBuf::from("/other/repo/ART-123"), ..key.clone()});
        assert_ne!(key, SessionKey {site: "https://other.example".into(), ..key.clone()});
    }
}
