mod command_palette;
mod issue_detail;
mod issue_links;
mod jira;
mod git;
mod git_ui;
mod preferences;
mod secrets;
mod transition_ui;
mod ui;

fn main() -> gtk::glib::ExitCode {
    ui::run()
}
