use std::{cell::{Cell, RefCell}, rc::Rc};

use adw::prelude::*;
use serde_json::{Map, Value};

use crate::jira::{IssueTransition, TransitionField};

pub type TransitionSubmit = Rc<dyn Fn(IssueTransition, Map<String, Value>, Map<String, Value>, Option<Value>) -> bool>;
pub type FormOpened = Rc<dyn Fn(TransitionForm)>;

#[derive(Clone)]
pub struct TransitionForm {
    pub key: String,
    pub window: gtk::Window,
    pub error: gtk::Label,
    pub submit: gtk::Button,
    pub progress: gtk::Spinner,
    fields_box: gtk::Box,
    fields_scroll: gtk::ScrolledWindow,
    controls: Rc<RefCell<Vec<FormField>>>,
    extra_fields: Vec<TransitionField>,
    worklog_recorded: Rc<Cell<bool>>,
    cloud: bool,
}

impl TransitionForm {
    pub fn show_error(&self, message: &str, worklog_recorded: bool, field_keys: &[String]) {
        if worklog_recorded {
            self.worklog_recorded.set(true);
        }
        let human_message = message
            .strip_prefix("Jira HTTP 400: ")
            .or_else(|| message.strip_prefix("Jira HTTP 422: "))
            .unwrap_or(message);
        self.error.set_text(human_message);
        self.error.set_visible(true);
        self.submit.set_sensitive(true);
        self.progress.stop();
        self.progress.set_visible(false);

        // Workflow validators may require fields that their metadata calls
        // optional. Respect the actual error rather than that stale flag.
        for item in self.controls.borrow_mut().iter_mut() {
            if field_in_errors(&item.field, field_keys) {
                item.field.required = true;
                item.label.set_text(&format!("{} *", item.field.name));
            }
        }
        let mut added = false;
        for mut field in referenced_fields(field_keys, &self.extra_fields) {
            if self.controls.borrow().iter().any(|existing| existing.field.id == field.id) {
                continue;
            }
            field.required = true;
            let (control, label) = append_field_row(&self.fields_box, &field, self.cloud);
            self.controls.borrow_mut().push(FormField {
                field, control, label, update_issue: true,
            });
            added = true;
        }
        if added {
            let scroll = self.fields_scroll.clone();
            gtk::glib::idle_add_local_once(move || {
                let adjustment = scroll.vadjustment();
                adjustment.set_value((adjustment.upper() - adjustment.page_size()).max(0.0));
            });
        }
    }

    pub fn close(&self) {
        self.window.close();
    }
}

struct FormField {
    field: TransitionField,
    control: FieldControl,
    label: gtk::Label,
    update_issue: bool,
}

fn field_in_errors(field: &TransitionField, field_keys: &[String]) -> bool {
    // The original error JSON supplies exact field keys, so no free-text
    // heuristics or substring searches are needed.
    field_keys.iter().any(|key| {
        key.eq_ignore_ascii_case(&field.id) || key.eq_ignore_ascii_case(&field.name)
    })
}

fn referenced_fields(field_keys: &[String], fields: &[TransitionField]) -> Vec<TransitionField> {
    fields.iter().filter(|field| field_in_errors(field, field_keys))
        .cloned().collect()
}

enum FieldControl {
    Text(gtk::Entry),
    Paragraph(gtk::TextView),
    Structured(gtk::TextView),
    Boolean(gtk::DropDown),
    Worklog { duration: gtk::Entry, note: gtk::TextView },
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
                activate_transition(
                    &parent, &key, cloud, transition.clone(), submit.clone(), on_open.clone(),
                );
            }
        });
        actions.insert(&button, -1);
    }
    container.append(&actions);
}

/// Execute a workflow action from either the inspector or the command palette.
/// Transitions with required fields use the same validation dialog in both places.
pub fn activate_transition(
    parent: &adw::ApplicationWindow,
    issue_key: &str,
    cloud: bool,
    transition: IssueTransition,
    submit: TransitionSubmit,
    on_open: FormOpened,
) {
    if transition_needs_form(&transition) {
        open_transition_form(parent, issue_key, cloud, transition, submit, on_open);
    } else {
        submit(transition, Map::new(), Map::new(), None);
    }
}

fn transition_needs_form(transition: &IssueTransition) -> bool {
    transition.fields.iter().any(|field| field.required)
}

pub fn open_transition_form(
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
        .default_width(660)
        .default_height(if transition.fields.len() >= 4 { 720 } else { 560 })
        .resizable(true)
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 14);
    root.set_margin_top(20);
    root.set_margin_bottom(20);
    root.set_margin_start(22);
    root.set_margin_end(22);

    let title = gtk::Label::new(Some(&format!("Move to {}", transition.to_name)));
    title.add_css_class("title-3");
    title.set_xalign(0.0);
    root.append(&title);

    let explanation = gtk::Label::new(Some("Complete the missing details to change the status."));
    explanation.add_css_class("dim-label");
    explanation.set_xalign(0.0);
    explanation.set_wrap(true);
    root.append(&explanation);

    let form = gtk::Box::new(gtk::Orientation::Vertical, 13);
    let controls = Rc::new(RefCell::new(Vec::<FormField>::new()));
    for field in &transition.fields {
        let (control, label) = append_field_row(&form, field, cloud);
        controls.borrow_mut().push(FormField {
            field: field.clone(),
            control,
            label,
            update_issue: false,
        });
    }
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_vexpand(true);
    scroller.set_min_content_height(180);
    scroller.set_child(Some(&form));
    root.append(&scroller);
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
    let controls_for_apply = controls.clone();
    let edit_fields_for_form = transition.edit_fields.clone();
    let worklog_recorded = Rc::new(Cell::new(false));
    let recorded_for_apply = worklog_recorded.clone();
    apply.connect_clicked(move |_| {
        error_apply.set_visible(false);
        let mut values = Map::new();
        let mut issue_edits = Map::new();
        let mut worklog = None;
        for item in controls_for_apply.borrow().iter() {
            match control_value(&item.field, &item.control, cloud) {
                Ok(Some(value)) => {
                    if item.field.is_worklog() {
                        if !recorded_for_apply.get() {
                            worklog = Some(value);
                        }
                    } else if item.update_issue {
                        issue_edits.insert(item.field.id.clone(), value);
                    } else {
                        values.insert(item.field.id.clone(), value);
                    }
                }
                Ok(None) => {}
                Err(message) => {
                    error_apply.set_text(&message);
                    error_apply.set_visible(true);
                    return;
                }
            }
        }
        if submit(transition.clone(), values, issue_edits, worklog) {
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
        fields_box: form,
        fields_scroll: scroller,
        controls,
        extra_fields: edit_fields_for_form,
        worklog_recorded,
        cloud,
    });
    dialog.present();
}

fn append_field_row(
    form: &gtk::Box, field: &TransitionField, cloud: bool,
) -> (FieldControl, gtk::Label) {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 5);
    let name = gtk::Label::new(Some(&format!(
        "{}{}", field.name, if field.required { " *" } else { "" },
    )));
    name.set_xalign(0.0);
    row.append(&name);
    let control = if field.is_worklog() {
        let label = gtk::Label::new(Some("Time spent"));
        label.set_xalign(0.0);
        row.append(&label);
        let duration = gtk::Entry::new();
        duration.set_placeholder_text(Some("e.g. 1h 30m"));
        row.append(&duration);
        let label = gtk::Label::new(Some("Description (optional)"));
        label.set_xalign(0.0);
        row.append(&label);
        let note = text_area("", false);
        row.append(&text_area_frame(&note));
        FieldControl::Worklog { duration, note }
    } else if !field.choices.is_empty() && field.schema_type == "array" {
        let mut checks = Vec::new();
        let choices = gtk::FlowBox::new();
        choices.set_selection_mode(gtk::SelectionMode::None);
        choices.set_max_children_per_line(3);
        choices.set_column_spacing(10);
        choices.set_row_spacing(7);
        for choice in &field.choices {
            let check = gtk::CheckButton::with_label(&choice.label);
            choices.insert(&check, -1);
            checks.push(check);
        }
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroll.set_min_content_height(75);
        scroll.set_max_content_height(200);
        scroll.set_propagate_natural_height(true);
        scroll.set_child(Some(&choices));
        row.append(&scroll);
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
        row.append(&select);
        FieldControl::Boolean(select)
    } else if field.is_multiline() {
        let input = text_area("", false);
        row.append(&text_area_frame(&input));
        FieldControl::Paragraph(input)
    } else if field.uses_structured_input() {
        let hint = gtk::Label::new(Some("Advanced Jira field · enter a JSON value"));
        hint.add_css_class("dim-label");
        hint.set_xalign(0.0);
        row.append(&hint);
        let input = text_area("", true);
        row.append(&text_area_frame(&input));
        FieldControl::Structured(input)
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
    form.append(&row);
    (control, name)
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
        FieldControl::Worklog { duration, note } => {
            let spent = duration.text();
            let spent = spent.trim();
            if spent.is_empty() {
                return if field.required {
                    Err("Enter the time spent (e.g. 1h 30m)".to_owned())
                } else {
                    Ok(None)
                };
            }
            if !valid_worklog_duration(spent) {
                return Err("Enter time like 45m, 2h or 1h 30m".to_owned());
            }
            let buffer = note.buffer();
            let message = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
            let mut payload = serde_json::json!({"timeSpent": spent});
            if !message.trim().is_empty() {
                payload["comment"] = if cloud {
                    serde_json::json!({
                        "type": "doc", "version": 1,
                        "content": [{
                            "type": "paragraph",
                            "content": [{"type": "text", "text": message.trim()}]
                        }]
                    })
                } else {
                    Value::String(message.trim().to_owned())
                };
            }
            Ok(Some(payload))
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

fn valid_worklog_duration(value: &str) -> bool {
    let mut parts = value.split_whitespace();
    let valid_part = |part: &str| {
        let Some((offset, _)) = part.char_indices().last() else {
            return false;
        };
        let (digits, unit) = part.split_at(offset);
        !digits.is_empty() && digits.bytes().all(|digit| digit.is_ascii_digit())
            && digits.parse::<u32>().is_ok_and(|amount| amount > 0)
            && matches!(unit, "w" | "d" | "h" | "m")
    };
    parts.next().is_some_and(valid_part) && parts.all(valid_part)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_dialog_only_when_transition_metadata_requires_input() {
        let required = TransitionField {
            id: "resolution".into(), name: "Resolution".into(),
            schema_type: "resolution".into(), item_type: String::new(),
            custom_type: String::new(), required: true, choices: Vec::new(),
        };
        let transition = IssueTransition {
            id: "31".into(), name: "Done".into(), to_name: "Done".into(),
            to_id: "3".into(), fields: vec![], edit_fields: vec![],
        };
        assert!(!transition_needs_form(&transition));
        let optional = TransitionField { required: false, ..required.clone() };
        let with_optional = IssueTransition { fields: vec![optional], ..transition.clone() };
        assert!(!transition_needs_form(&with_optional));
        let with_required = IssueTransition { fields: vec![required], ..transition };
        assert!(transition_needs_form(&with_required));
    }

    #[test]
    fn worklog_duration_has_normal_human_input() {
        for text in ["45m", "2h", "1h 30m", "1d 2h 5m"] {
            assert!(valid_worklog_duration(text), "{text}");
        }
        for text in ["", "{}", "0m", "30", "garbage", "😀"] {
            assert!(!valid_worklog_duration(text), "{text}");
        }
    }

    #[test]
    fn jira_error_matching_uses_only_exact_structured_field_keys() {
        let fix = TransitionField {
            id: "fixVersions".into(), name: "Fix versions".into(),
            schema_type: "array".into(), item_type: "version".into(),
            custom_type: String::new(), required: false, choices: vec![],
        };
        let affects = TransitionField {
            id: "versions".into(), name: "Affects versions".into(),
            ..fix.clone()
        };
        let impact = TransitionField {
            id: "customfield_10135".into(), name: "Impact Assessment".into(),
            ..fix.clone()
        };
        assert!(field_in_errors(&fix, &["fixVersions".into()]));
        assert!(!field_in_errors(&affects, &["fixVersions".into()]));
        assert!(field_in_errors(&impact, &["customfield_10135".into()]));
        assert!(field_in_errors(&impact, &["Impact Assessment".into()]));
        assert!(!field_in_errors(&impact, &[
            "Please fill the impact assessment and fix versions fields".into()
        ]));
        assert!(referenced_fields(&[], &[fix, affects, impact]).is_empty());
    }

}

