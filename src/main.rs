mod issue_detail;
mod jira;
mod preferences;
mod secrets;
mod transition_ui;
mod ui;

fn main() -> gtk::glib::ExitCode {
    ui::run()
}
