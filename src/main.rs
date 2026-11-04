mod jira;
mod secrets;
mod ui;

fn main() -> gtk::glib::ExitCode {
    ui::run()
}
