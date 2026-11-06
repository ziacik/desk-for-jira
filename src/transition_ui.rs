use std::rc::Rc;

use adw::prelude::*;
use serde_json::{Map, Value};

use crate::jira::{IssueTransition, TransitionField};

pub type TransitionSubmit = Rc<dyn Fn(IssueTransition, Map<String, Value>) -> bool>;
pub type FormOpened = Rc<dyn Fn(TransitionForm)>;

#[derive(Clone)]
pub struct TransitionForm {
    pub key: String,
    pub window: gtk::Window,
    pub error: gtk::Label,
    pub submit: gtk::Button,
    pub progress: gtk::Spinner,
}

impl TransitionForm {
    pub fn show_error(&self, message: &str) {
        self.error.set_text(message);
        self.error.set_visible(true);
        self.submit.set_sensitive(true);
        self.progress.stop();
        self.progress.set_visible(false);
    }

    pub fn close(&self) {
        self.window.close();
    }
}

enum FieldControl {
    Text(gtk::Entry),
    Paragraph(gtk::TextView),
    Structured(gtk::TextView),
    Boolean(gtk::DropDown),
    Single(gtk::DropDown),
    Multiple(Vec<gtk::CheckButton>),
}

fn clear(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

pub fn render_actions(
    container: &gtk::Box,
    parent: &adw::ApplicationWindow,
    issue_key: &str,
    cloud: bool,
    transitions: Vec<IssueTransition>,
    submit: TransitionSubmit,
    on_open: FormOpened,
) {
    clear(container);
    container.set_visible(true);
    let heading = gtk::Label::new(Some("TRANSITIONS"));
    heading.add_css_class("detail-actions-heading");
    heading.set_xalign(0.0);
    container.append(&heading);

    if transitions.is_empty() {
        let message = gtk::Label::new(Some("No available workflow actions"));
        message.add_css_class("dim-label");
        message.set_xalign(0.0);
        container.append(&message);
        return;
    }

    let actions = gtk::FlowBox::new();
    actions.set_selection_mode(gtk::SelectionMode::None);
    actions.set_max_children_per_line(3);
    actions.set_column_spacing(7);
    actions.set_row_spacing(7);
    actions.set_halign(gtk::Align::Start);
    for transition in transitions {
        let button = gtk::Button::with_label(&transition.name);
        button.add_css_class("transition-action");
        button.set_tooltip_text(Some(&format!("Move to {}", transition.to_name)));
        let parent = parent.downgrade();
        let key = issue_key.to_owned();
        let submit = submit.clone();
        let on_open = on_open.clone();
        button.connect_clicked(move |_| {
            if let Some(parent) = parent.upgrade() {
                open_transition_form(
                    &parent, &key, cloud, transition.clone(), submit.clone(), on_open.clone(),
                );
            }
        });
        actions.insert(&button, -1);
    }
    container.append(&actions);
}

fn open_transition_form(
    parent: &adw::ApplicationWindow,
    issue_key: &str,
    cloud: bool,
    transition: IssueTransition,
    submit: TransitionSubmit,
    on_open: FormOpened,
) {
    let dialog = gtk::Window::builder()
        .title(format!("{} · {}", issue_key, transition.name))
        .transient_for(parent)
        .modal(true)
        .default_width(500)
        .default_height(if transition.fields.is_empty() { 240 } else { 420 })
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 15);
    root.set_margin_top(20);
    root.set_margin_bottom(20);
    root.set_margin_start(22);
    root.set_margin_end(22);

    let title = gtk::Label::new(Some(&format!("Move to {}", transition.to_name)));
    title.add_css_class("title-3");
    title.set_xalign(0.0);
    root.append(&title);

    let explanation = gtk::Label::new(Some(
        if transition.fields.is_empty() {
            "Jira did not provide editable fields for this transition. Confirm the status change below."
        } else {
            "Set any fields needed for this transition. Required fields are marked with *."
        },
    ));
    explanation.add_css_class("dim-label");
    explanation.set_xalign(0.0);
    explanation.set_wrap(true);
    root.append(&explanation);

    let form = gtk::Box::new(gtk::Orientation::Vertical, 13);
    let mut controls = Vec::new();
    for field in &transition.fields {
        let row = gtk::Box::new(gtk::Orientation::Vertical, 5);
        let name = gtk::Label::new(Some(&format!(
            "{}{}", field.name, if field.required { " *" } else { "" },
        )));
        name.set_xalign(0.0);
        row.append(&name);
        let control = if !field.choices.is_empty() && field.schema_type == "array" {
            let mut checks = Vec::new();
            for choice in &field.choices {
                let check = gtk::CheckButton::with_label(&choice.label);
                row.append(&check);
                checks.push(check);
            }
            FieldControl::Multiple(checks)
        } else if !field.choices.is_empty() {
            let labels = std::iter::once("Select a value…")
                .chain(field.choices.iter().map(|choice| choice.label.as_str()))
                .collect::<Vec<_>>();
            let select = gtk::DropDown::from_strings(&labels);
            select.set_hexpand(true);
            row.append(&select);
            FieldControl::Single(select)
        } else if field.schema_type == "boolean" {
            let select = gtk::DropDown::from_strings(&["No change", "No", "Yes"]);
            if field.required {
                select.set_selected(0);
            }
            row.append(&select);
            FieldControl::Boolean(select)
        } else if field.uses_structured_input() {
            let hint = gtk::Label::new(Some("Advanced Jira field · enter a JSON value"));
            hint.add_css_class("dim-label");
            hint.set_xalign(0.0);
            row.append(&hint);
            let input = text_area("", true);
            row.append(&text_area_frame(&input));
            FieldControl::Structured(input)
        } else if field.is_multiline() {
            let input = text_area("", false);
            row.append(&text_area_frame(&input));
            FieldControl::Paragraph(input)
        } else {
            let input = gtk::Entry::new();
            let placeholder = match field.schema_type.as_str() {
                "date" => "YYYY-MM-DD",
                "datetime" => "2026-11-06T15:30:00.000+0100",
                "number" => "Enter a number",
                "user" if cloud => "Atlassian account ID",
                "user" => "Jira username",
                "option" | "resolution" | "priority" | "version"
                    | "component" | "issuetype" | "project" => "Jira field value ID",
                "array" if field.item_type == "string" => "Comma-separated values",
                "array" => "Comma-separated Jira value IDs",
                _ => "Enter a value",
            };
            input.set_placeholder_text(Some(placeholder));
            input.set_hexpand(true);
            row.append(&input);
            FieldControl::Text(input)
        };
        controls.push((field.clone(), control));
        form.append(&row);
    }
    if !transition.fields.is_empty() {
        let scroller = gtk::ScrolledWindow::new();
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroller.set_vexpand(true);
        scroller.set_child(Some(&form));
        root.append(&scroller);
    }

    let error = gtk::Label::new(None);
    error.add_css_class("error");
    error.set_wrap(true);
    error.set_xalign(0.0);
    error.set_visible(false);
    root.append(&error);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let dialog_cancel = dialog.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(dialog) = dialog_cancel.upgrade() { dialog.close(); }
    });
    actions.append(&cancel);
    let progress = gtk::Spinner::new();
    progress.set_visible(false);
    actions.append(&progress);
    let apply = gtk::Button::with_label(&transition.name);
    apply.add_css_class("suggested-action");
    actions.append(&apply);
    root.append(&actions);
    let progress_apply = progress.clone();
    let apply_button = apply.clone();
    let error_apply = error.clone();
    apply.connect_clicked(move |_| {
        error_apply.set_visible(false);
        let mut values = Map::new();
        for (field, control) in &controls {
            match control_value(field, control, cloud) {
                Ok(Some(value)) => { values.insert(field.id.clone(), value); }
                Ok(None) => {}
                Err(message) => {
                    error_apply.set_text(&message);
                    error_apply.set_visible(true);
                    return;
                }
            }
        }
        if submit(transition.clone(), values) {
            apply_button.set_sensitive(false);
            progress_apply.set_visible(true);
            progress_apply.start();
        }
    });
    dialog.set_child(Some(&root));
    on_open(TransitionForm {
        key: issue_key.to_owned(),
        window: dialog.clone(),
        error,
        submit: apply,
        progress,
    });
    dialog.present();
}

fn text_area(initial: &str, monospace: bool) -> gtk::TextView {
    let input = gtk::TextView::new();
    input.set_wrap_mode(gtk::WrapMode::WordChar);
    input.set_top_margin(9);
    input.set_bottom_margin(9);
    input.set_left_margin(10);
    input.set_right_margin(10);
    input.set_monospace(monospace);
    input.buffer().set_text(initial);
    input
}

fn text_area_frame(input: &gtk::TextView) -> gtk::ScrolledWindow {
    let frame = gtk::ScrolledWindow::new();
    frame.add_css_class("frame");
    frame.set_min_content_height(96);
    frame.set_max_content_height(160);
    frame.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    frame.set_child(Some(input));
    frame
}

fn control_value(
    field: &TransitionField,
    control: &FieldControl,
    cloud: bool,
) -> Result<Option<Value>, String> {
    match control {
        FieldControl::Text(input) => {
            let text = input.text();
            text_value_or_none(field, &text, cloud)
        }
        FieldControl::Paragraph(input) | FieldControl::Structured(input) => {
            let buffer = input.buffer();
            let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
            text_value_or_none(field, &text, cloud)
        }
        FieldControl::Boolean(select) => match select.selected() {
            0 if field.required => Err(format!("{} is required", field.name)),
            0 => Ok(None),
            1 => Ok(Some(Value::Bool(false))),
            2 => Ok(Some(Value::Bool(true))),
            _ => Err(format!("Invalid value for {}", field.name)),
        },
        FieldControl::Single(dropdown) => {
            let selected = dropdown.selected();
            if selected == 0 {
                return if field.required {
                    Err(format!("{} is required", field.name))
                } else {
                    Ok(None)
                };
            }
            let Some(choice) = field.choices.get(selected as usize - 1) else {
                return Err(format!("Invalid value for {}", field.name));
            };
            Ok(Some(choice.value.clone()))
        }
        FieldControl::Multiple(checks) => {
            let selected: Vec<Value> = checks.iter().zip(&field.choices)
                .filter(|(check, _)| check.is_active())
                .map(|(_, choice)| choice.value.clone())
                .collect();
            if selected.is_empty() {
                return if field.required {
                    Err(format!("{} is required", field.name))
                } else {
                    Ok(None)
                };
            }
            Ok(Some(Value::Array(selected)))
        }
    }
}

fn text_value_or_none(
    field: &TransitionField,
    value: &str,
    cloud: bool,
) -> Result<Option<Value>, String> {
    if value.trim().is_empty() && !field.required {
        return Ok(None);
    }
    field.text_value(value, cloud)
        .map(Some)
        .map_err(|error| error.to_string())
}
