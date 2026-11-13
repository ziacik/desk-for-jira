mod codex;
mod command_palette;
mod issue_detail;
mod issue_links;
mod jira;
mod git;
mod git_ui;
mod all_worktrees;
mod development;
mod pull_requests;
mod preferences;
mod secrets;
mod transition_ui;
mod ui;
mod zoom;

fn main() -> gtk::glib::ExitCode {
    ui::run()
}
