use std::{cell::{Cell, RefCell}, collections::{HashMap, HashSet}, rc::Rc, sync::Arc};

use adw::prelude::*;
use serde_json::{Map, Value};

use crate::jira::{IssueTransition, TransitionField, WorkflowGraph};

pub type TransitionSubmit = Rc<dyn Fn(IssueTransition, Map<String, Value>, Map<String, Value>, Option<Value>) -> bool>;
pub type FormOpened = Rc<dyn Fn(TransitionForm)>;
pub type PrimaryChanged = Rc<dyn Fn(Option<String>)>;

pub struct WorkflowContext {
    pub current_status_id: String,
    pub graph: Arc<WorkflowGraph>,
    pub terminal_status_ids: HashSet<String>,
}

pub struct ActionOptions {
    pub workflow: Option<WorkflowContext>,
    pub preferred_id: Option<String>,
    pub on_primary_changed: Option<PrimaryChanged>,
    pub submit: TransitionSubmit,
    pub on_open: FormOpened,
}

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

/// Primary is the directed step that most reduces the distance to a
/// terminal status, computed by reverse BFS over the *actual* workflow graph.
/// Status IDs are opaque keys, never numbers for sorting.
fn primary_transition_index(
    transitions: &[IssueTransition],
    _cloud_sorted: bool,
    override_id: Option<&str>,
    workflow: Option<&WorkflowContext>,
) -> Option<usize> {
    if let Some(index) = override_id.and_then(|id| transitions.iter().position(|t| t.id == id)) {
        return Some(index);
    }
    let context = workflow?;
    let done_distances = context.graph.distance_to_terminals(&context.terminal_status_ids);
    // Some Jira workflows close via a GLOBAL transition from every status.
    // In that case the directed graph has no path to Done; its furthest
    // reachable normal stage is the target until the final Close action.
    let frontier = context.graph.normal_frontier(&context.terminal_status_ids);
    let frontier_distances = context.graph.distance_to_terminals(&frontier);
    let has_forward = |distances: &HashMap<String, usize>| {
        let Some(current) = distances.get(&context.current_status_id) else {
            return false;
        };
        transitions.iter().any(|transition| {
            !context.terminal_status_ids.contains(&transition.to_id)
                && context.graph.has_edge(&context.current_status_id, &transition.to_id)
                && distances.get(&transition.to_id).is_some_and(|next| next < current)
        })
    };
    let frontier_distance = frontier_distances.get(&context.current_status_id).copied();
    let use_frontier = frontier_distance.is_some_and(|distance| {
        !done_distances.contains_key(&context.current_status_id)
            || (!has_forward(&done_distances)
                && (has_forward(&frontier_distances) || distance == 0))
    });
    let distances = if use_frontier { &frontier_distances } else { &done_distances };
    let current_distance = distances.get(&context.current_status_id).copied()?;
    if use_frontier && current_distance == 0 {
        let finish: Vec<usize> = transitions.iter().enumerate()
            .filter_map(|(index, transition)| (
                context.terminal_status_ids.contains(&transition.to_id)
            ).then_some(index))
            .collect();
        return (finish.len() == 1).then_some(finish[0]);
    }

    // Only directed workflow edges count. Global actions (such as an
    // unconditional Close) are not normal workflow progression.
    let mut local: Vec<usize> = transitions.iter().enumerate()
        .filter_map(|(index, transition)| (
            transition.is_global != Some(true)
            && transition.to_id != context.current_status_id
            && context.graph.has_edge(&context.current_status_id, &transition.to_id)
            && distances.contains_key(&transition.to_id)
        ).then_some(index))
        .collect();

    let progress: Vec<usize> = local.iter().copied()
        .filter(|&index| {
            let destination = &transitions[index].to_id;
            !context.terminal_status_ids.contains(destination)
                && distances.get(destination).is_some_and(|distance| *distance < current_distance)
        }).collect();
    if !progress.is_empty() {
        local = progress;
    } else {
        // Only promote a terminal action when no other nonterminal directed
        // edge leads to the terminal. Otherwise early Close shortcuts could
        // appear primary in cyclical workflows.
        if local.iter().any(|&index| !context.terminal_status_ids.contains(&transitions[index].to_id)) {
            return None;
        }
        local.retain(|&index| context.terminal_status_ids.contains(&transitions[index].to_id));
    }

    let nearest = local.iter()
        .filter_map(|&index| distances.get(&transitions[index].to_id))
        .copied().min()?;
    local.retain(|&index| distances.get(&transitions[index].to_id) == Some(&nearest));
    if local.len() == 1 {
        return local.first().copied();
    }

    // Equal shortest-path distances describe equally plausible branches.
    // Jira's menu priority cannot establish a primary workflow direction.
    None
}

fn action_button(
    parent: &adw::ApplicationWindow,
    issue_key: &str,
    cloud: bool,
    transition: IssueTransition,
    submit: &TransitionSubmit,
    on_open: &FormOpened,
) -> gtk::Button {
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
    button
}

pub fn render_actions(
    container: &gtk::Box,
    parent: &adw::ApplicationWindow,
    issue_key: &str,
    cloud: bool,
    transitions: Vec<IssueTransition>,
    options: ActionOptions,
) {
    let ActionOptions { workflow, preferred_id, on_primary_changed, submit, on_open } = options;
    clear(container);
    container.set_visible(true);

    if transitions.is_empty() {
        let message = gtk::Label::new(Some("No available workflow actions"));
        message.add_css_class("dim-label");
        message.set_xalign(0.0);
        container.append(&message);
        return;
    }

    let primary = primary_transition_index(
        &transitions, cloud, preferred_id.as_deref(), workflow.as_ref(),
    );
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.set_halign(gtk::Align::Start);
    row.add_css_class("workflow-actions-row");
    if let Some(index) = primary {
        let button = action_button(
            parent, issue_key, cloud, transitions[index].clone(), &submit, &on_open,
        );
        button.add_css_class("transition-primary");
        row.append(&button);
    }

    if transitions.len() > 1 || primary.is_none() {
        let popover = gtk::Popover::new();
        let list = gtk::Box::new(gtk::Orientation::Vertical, 3);
        list.add_css_class("workflow-actions-menu");
        list.set_margin_top(7);
        list.set_margin_bottom(7);
        list.set_margin_start(7);
        list.set_margin_end(7);
        for (index, transition) in transitions.iter().enumerate() {
            if Some(index) == primary {
                continue;
            }
            let item = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            let button = action_button(
                parent, issue_key, cloud, transition.clone(), &submit, &on_open,
            );
            button.add_css_class("flat");
            button.add_css_class("transition-secondary");
            button.set_hexpand(true);
            let popover_for_action = popover.clone();
            button.connect_clicked(move |_| popover_for_action.popdown());
            item.append(&button);

            if let Some(change) = &on_primary_changed {
                let choose = gtk::Button::from_icon_name("starred-symbolic");
                choose.add_css_class("flat");
                choose.add_css_class("transition-prefer");
                choose.set_tooltip_text(Some("Make primary action for this workflow status"));
                let id = transition.id.clone();
                let change = change.clone();
                let popover_for_change = popover.clone();
                choose.connect_clicked(move |_| {
                    popover_for_change.popdown();
                    change(Some(id.clone()));
                });
                item.append(&choose);
            }
            list.append(&item);
        }
        if preferred_id.is_some() && let Some(change) = &on_primary_changed {
            let reset = gtk::Button::with_label("Use Jira's default priority");
            reset.add_css_class("flat");
            reset.add_css_class("transition-reset");
            let change = change.clone();
            let popover_for_reset = popover.clone();
            reset.connect_clicked(move |_| {
                popover_for_reset.popdown();
                change(None);
            });
            list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            list.append(&reset);
        }
        popover.set_child(Some(&list));
        let more = gtk::MenuButton::new();
        more.add_css_class("workflow-actions-more");
        if primary.is_some() {
            more.set_icon_name("pan-down-symbolic");
            more.set_tooltip_text(Some("Other workflow actions"));
        } else {
            more.set_label("Workflow actions");
        }
        more.set_popover(Some(&popover));
        row.append(&more);
    }
    container.append(&row);
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
            to_id: "3".into(), to_category: None, is_global: None,
            opsbar_sequence: None, fields: vec![], edit_fields: vec![],
        };
        assert!(!transition_needs_form(&transition));
        let optional = TransitionField { required: false, ..required.clone() };
        let with_optional = IssueTransition { fields: vec![optional], ..transition.clone() };
        assert!(!transition_needs_form(&with_optional));
        let with_required = IssueTransition { fields: vec![required], ..transition };
        assert!(transition_needs_form(&with_required));
    }

    fn t(id: &str, is_global: Option<bool>, priority: Option<i64>) -> IssueTransition {
        IssueTransition {
            id: id.into(), name: format!("Localized {id}"), to_name: format!("State {id}"),
            to_id: id.into(), to_category: None, is_global,
            opsbar_sequence: priority, fields: vec![], edit_fields: vec![],
        }
    }

    fn workflow(
        current: &str, edges: &[(&str, &str)], terminal: &str,
    ) -> WorkflowContext {
        let mut graph = WorkflowGraph::default();
        graph.status_ids.insert(current.into());
        graph.status_ids.insert(terminal.into());
        for (source, target) in edges {
            graph.status_ids.insert((*source).into());
            graph.status_ids.insert((*target).into());
            graph.edges.entry((*source).into()).or_default().insert((*target).into());
        }
        WorkflowContext {
            current_status_id: current.into(),
            graph: Arc::new(graph),
            terminal_status_ids: HashSet::from([terminal.into()]),
        }
    }

    #[test]
    fn art_1885_begin_work_is_primary_not_stash_or_global_close() {
        // Ready for Dev -> Open is a rollback; the Jira opsbar lists it first.
        let mut stash = t("211", Some(false), Some(1));
        stash.to_id = "1".into();
        let mut begin_work = t("11", Some(false), Some(20));
        begin_work.to_id = "3".into();
        let mut close = t("141", Some(true), Some(10));
        close.to_id = "6".into();
        let context = workflow("10291", &[
            ("10291","1"),("1","10291"),("10291","3"),
            ("3","10053"),("10053","10069"),("10069","10054"),
            ("10054","6"),
        ], "6");
        assert_eq!(
            primary_transition_index(&[stash,begin_work,close], true, None, Some(&context)),
            Some(1),
        );
    }

    #[test]
    fn art_2921_review_is_primary_not_pause_work() {
        let mut pause = t("161", Some(false), Some(1));
        pause.to_id = "10291".into();
        let mut review = t("121", Some(false), Some(20));
        review.to_id = "10053".into();
        let mut close = t("141", Some(true), Some(10));
        close.to_id = "6".into();
        let context = workflow("3", &[
            ("10291","3"),("3","10291"),("3","10053"),
            ("10053","10069"),("10069","10054"),("10054","6"),
        ], "6");
        assert_eq!(
            primary_transition_index(&[pause,review,close],true,None,Some(&context)),Some(1),
        );
    }

    #[test]
    fn global_close_only_still_selects_normal_steps_then_close_at_frontier() {
        let mut start = t("begin", None, Some(25));
        start.to_id = "progress".into();
        let mut stash = t("stash", None, Some(1));
        stash.to_id = "open".into();
        let mut close = t("close", Some(true), Some(2));
        close.to_id = "done".into();
        let mut ready = workflow("ready", &[
            ("open","ready"),("ready","open"),("ready","progress"),
            ("progress","review"),("review","testing"),("testing","review"),
        ], "done");
        let graph = Arc::make_mut(&mut ready.graph);
        graph.initial_status_ids.insert("open".into());
        assert_eq!(
            primary_transition_index(&[stash,start,close.clone()],true,None,Some(&ready)),
            Some(1),
        );
        let mut review = t("review", None, None);
        review.to_id = "review".into();
        let mut pause = t("pause", None, None);
        pause.to_id = "ready".into();
        let progressing = WorkflowContext {
            current_status_id: "progress".into(), graph: ready.graph.clone(),
            terminal_status_ids: ready.terminal_status_ids.clone(),
        };
        assert_eq!(
            primary_transition_index(&[pause,review,close.clone()],true,None,Some(&progressing)),
            Some(1),
        );
        let testing = WorkflowContext {
            current_status_id: "testing".into(), graph: ready.graph.clone(),
            terminal_status_ids: ready.terminal_status_ids.clone(),
        };
        assert_eq!(primary_transition_index(&[close],true,None,Some(&testing)),Some(0));
    }

    #[test]
    fn terminal_step_is_primary_only_at_end_of_normal_workflow() {
        let mut close = t("finish", Some(false), None);
        close.to_id = "done".into();
        let context = workflow("testing", &[("testing","done")], "done");
        assert_eq!(primary_transition_index(&[close],true,None,Some(&context)),Some(0));
    }

    #[test]
    fn terminal_shortcut_cannot_override_a_forward_step() {
        let mut close = t("finish", None, Some(1));
        close.to_id = "done".into();
        let mut next = t("next", None, Some(50));
        next.to_id = "review".into();
        let context = workflow("progress", &[
            ("progress","done"),("progress","review"),("review","done")
        ],"done");
        // Direct Close makes the shortest distance 1. Choosing that shortcut
        // would be wrong, so the selector stays neutral rather than guess.
        assert_eq!(primary_transition_index(&[close,next],true,None,Some(&context)),None);
    }

    #[test]
    fn competing_forward_paths_use_distance_to_terminal() {
        let mut long_way = t("long", None, Some(1));
        long_way.to_id = "a".into();
        let mut short_way = t("short", None, Some(20));
        short_way.to_id = "b".into();
        let context = workflow("start", &[
            ("start","a"),("start","b"),("a","x"),("x","y"),
            ("y","done"),("b","done"),
        ], "done");
        assert_eq!(
            primary_transition_index(&[long_way,short_way],true,None,Some(&context)),
            Some(1),
        );
    }

    #[test]
    fn equal_distance_branches_do_not_invent_primary() {
        let mut first = t("a", None, None);
        first.to_id = "left".into();
        let mut second = t("b", None, None);
        second.to_id = "right".into();
        let context = workflow("start", &[
            ("start","left"),("start","right"),("left","done"),("right","done"),
        ], "done");
        assert_eq!(
            primary_transition_index(&[first.clone(),second.clone()],true,None,Some(&context)),
            None,
        );
        assert_eq!(
            primary_transition_index(&[first,second],true,Some("b"),Some(&context)),
            Some(1),
        );
    }

    #[test]
    fn no_directed_workflow_metadata_never_promotes_unknown_action() {
        let context = workflow("ready",&[], "done");
        assert_eq!(primary_transition_index(&[t("action",None,None)],true,None,Some(&context)),None);
        assert_eq!(primary_transition_index(&[t("action",None,None)],true,Some("action"),None),Some(0));
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

