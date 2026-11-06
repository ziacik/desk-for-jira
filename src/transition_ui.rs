use std::rc::Rc;

use adw::prelude::*;
use serde_json::{Map, Value};

use crate::jira::{IssueTransition, TransitionField};

pub type TransitionSubmit = Rc<dyn Fn(IssueTransition, Map<String, Value>)>;

enum FieldControl {
    Text(gtk::Entry),
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
    base_url: &str,
    cloud: bool,
    transitions: Vec<IssueTransition>,
    submit: TransitionSubmit,
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
        let base_url = base_url.to_owned();
        let submit = submit.clone();
        button.connect_clicked(move |_| {
            if transition.required_fields.is_empty() {
                submit(transition.clone(), Map::new());
            } else if let Some(parent) = parent.upgrade() {
                open_transition_form(&parent, &key, &base_url, cloud, transition.clone(), submit.clone());
            }
        });
        actions.insert(&button, -1);
    }
    container.append(&actions);
}

fn open_transition_form(
    parent: &adw::ApplicationWindow,
    issue_key: &str,
    base_url: &str,
    cloud: bool,
    transition: IssueTransition,
    submit: TransitionSubmit,
) {
    let dialog = gtk::Window::builder()
        .title(format!("{} · {}", issue_key, transition.name))
        .transient_for(parent)
        .modal(true)
        .default_width(460)
        .default_height(350)
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

    let unsupported: Vec<_> = transition.required_fields.iter()
        .filter(|field| !field.is_supported())
        .map(|field| field.name.as_str()).collect();

    if !unsupported.is_empty() {
        let description = gtk::Label::new(Some(&format!(
            "This transition requires fields that cannot be edited here: {}. Complete it in Jira.",
            unsupported.join(", ")
        )));
        description.set_wrap(true);
        description.set_xalign(0.0);
        root.append(&description);
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        let dialog_cancel = dialog.downgrade();
        cancel.connect_clicked(move |_| {
            if let Some(dialog) = dialog_cancel.upgrade() { dialog.close(); }
        });
        actions.append(&cancel);
        let open = gtk::Button::with_label("Open in Jira");
        open.add_css_class("suggested-action");
        let url = format!("{}/browse/{issue_key}", base_url.trim_end_matches('/'));
        let dialog_open = dialog.downgrade();
        open.connect_clicked(move |_| {
            let _ = gtk::gio::AppInfo::launch_default_for_uri(
                &url, None::<&gtk::gio::AppLaunchContext>,
            );
            if let Some(dialog) = dialog_open.upgrade() { dialog.close(); }
        });
        actions.append(&open);
        root.append(&actions);
        dialog.set_child(Some(&root));
        dialog.present();
        return;
    }

    let explanation = gtk::Label::new(Some("Fill in the required fields to complete this transition."));
    explanation.add_css_class("dim-label");
    explanation.set_xalign(0.0);
    explanation.set_wrap(true);
    root.append(&explanation);

    let form = gtk::Box::new(gtk::Orientation::Vertical, 13);
    let mut controls = Vec::new();
    for field in &transition.required_fields {
        let row = gtk::Box::new(gtk::Orientation::Vertical, 5);
        let name = gtk::Label::new(Some(&format!("{} *", field.name)));
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
        } else {
            let input = gtk::Entry::new();
            let placeholder = match field.schema_type.as_str() {
                "date" => "YYYY-MM-DD",
                "number" => "Enter a number",
                "array" => "Comma-separated values",
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
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_vexpand(true);
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
    let apply = gtk::Button::with_label(&transition.name);
    apply.add_css_class("suggested-action");
    actions.append(&apply);
    root.append(&actions);
    let dialog_apply = dialog.downgrade();
    apply.connect_clicked(move |_| {
        let mut values = Map::new();
        for (field, control) in &controls {
            let result = control_value(field, control, cloud);
            match result {
                Ok(value) => { values.insert(field.id.clone(), value); }
                Err(message) => {
                    error.set_text(&message);
                    error.set_visible(true);
                    return;
                }
            }
        }
        submit(transition.clone(), values);
        if let Some(dialog) = dialog_apply.upgrade() { dialog.close(); }
    });
    dialog.set_child(Some(&root));
    dialog.present();
}

fn control_value(field: &TransitionField, control: &FieldControl, cloud: bool) -> Result<Value, String> {
    match control {
        FieldControl::Text(input) => field.text_value(input.text().as_str(), cloud)
            .map_err(|error| error.to_string()),
        FieldControl::Single(dropdown) => {
            let selected = dropdown.selected();
            if selected == 0 {
                return Err(format!("{} is required", field.name));
            }
            let Some(choice) = field.choices.get(selected as usize - 1) else {
                return Err(format!("Invalid value for {}", field.name));
            };
            Ok(choice.value.clone())
        }
        FieldControl::Multiple(checks) => {
            let selected: Vec<Value> = checks.iter().zip(&field.choices)
                .filter_map(|(check, choice)| check.is_active().then(|| choice.value.clone()))
                .collect();
            if selected.is_empty() {
                return Err(format!("{} is required", field.name));
            }
            Ok(Value::Array(selected))
        }
    }
}
