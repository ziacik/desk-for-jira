mod jira;
mod preferences;
mod secrets;
mod ui;

fn main() -> gtk::glib::ExitCode {
    ui::run()
}
